//! Fixed read-only ripgrep operation. Unlike exec_command, this contract cannot
//! select a program, shell, environment overrides, preprocessors or write flags.
use std::collections::BTreeMap;
use std::time::Duration;

use codex_tools::{JsonSchema, JsonToolOutput, ResponsesApiTool, ToolName, ToolSpec};
use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode};
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::{ToolInvocation, ToolPayload, boxed_tool_output};
use crate::tools::handlers::{parse_arguments, wait_for_tool_environment};
use crate::tools::registry::{CoreToolRuntime, ToolExecutor};

pub(crate) struct FileSearchHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    paths: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    globs: Vec<String>,
    #[serde(default)]
    fixed: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    ascii_case: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    no_ignore: bool,
    #[serde(default)]
    inventory: bool,
    #[serde(default)]
    json: bool,
    max_count: Option<usize>,
}

impl Arguments {
    fn argv(&self) -> Result<Vec<String>, FunctionCallError> {
        let valid = |values: &[String]| {
            values
                .iter()
                .all(|value| !value.is_empty() && !value.contains('\0') && value.len() <= 32768)
        };
        if self.paths.is_empty()
            || !valid(&self.paths)
            || self.paths.iter().any(|p| p == "-")
            || !valid(&self.globs)
            || !valid(&self.patterns)
            || self.patterns.len() > 64
            || (!self.inventory && self.patterns.is_empty())
            || (self.inventory
                && (!self.patterns.is_empty() || self.json || self.max_count.is_some()))
            || (self.ascii_case && !self.fixed)
            || self
                .max_count
                .is_some_and(|n| n == 0 || n > 1001 || !self.json)
            || (self.fixed && self.patterns.iter().any(|p| p.contains('\n')))
        {
            return Err(FunctionCallError::RespondToModel(
                "invalid bounded file-search arguments".into(),
            ));
        }
        let mut args = vec!["--no-config", "--null", "--path-separator", "/"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if self.inventory {
            args.push("--files".into());
        } else {
            args.extend(
                ["--text", "--color", "never", "--with-filename"]
                    .into_iter()
                    .map(str::to_owned),
            );
            args.push(
                if self.ignore_case {
                    "--ignore-case"
                } else {
                    "--case-sensitive"
                }
                .into(),
            );
            if self.ascii_case {
                args.push("--no-unicode".into());
            }
            if self.fixed {
                args.push("--fixed-strings".into());
            }
            args.push(if self.json { "--json" } else { "--count" }.into());
            if let Some(count) = self.max_count {
                args.extend(["--max-count".into(), count.to_string()]);
            }
            for pattern in &self.patterns {
                args.extend(["--regexp".into(), pattern.clone()]);
            }
        }
        if self.hidden {
            args.push("--hidden".into());
        }
        if self.no_ignore {
            args.push("--no-ignore".into());
        }
        for glob in &self.globs {
            args.extend(["--glob".into(), glob.clone()]);
        }
        args.push("--".into());
        args.extend(self.paths.iter().cloned());
        Ok(args)
    }
}

impl ToolExecutor<ToolInvocation> for FileSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("file_search")
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let strings = || JsonSchema::array(JsonSchema::string(None), None);
        let mut properties = BTreeMap::from([
            ("paths".into(), strings()), ("patterns".into(), strings()), ("globs".into(), strings()),
            ("max_count".into(), JsonSchema::integer(Some("JSON mode only: 1..1001 matching lines per file, including any caller lookahead.".into()))),
        ]);
        for name in [
            "fixed",
            "ignore_case",
            "ascii_case",
            "hidden",
            "no_ignore",
            "inventory",
            "json",
        ] {
            properties.insert(name.into(), JsonSchema::boolean(None));
        }
        ToolSpec::Function(ResponsesApiTool {
            name: "file_search".into(),
            description: "Read-only search backend for search_files. Prefer that helper for parsed counts, coverage, hydration and pagination. Fixed ripgrep argv: nonempty paths, 1-64 patterns (none for inventory), and ordered globs; original helper path/glob cardinality is preserved. No shell, caller-selected executable, preprocessing, config file, or write options. Returns exact stdout/stderr and exit_code (0 matches, 1 no match, 2 partial/error), not a command handle. Local full-disk-read environments only, as for native Git inspection. Capture is bounded to 8 MiB per stream and 60 seconds; overflow, invalid UTF-8, cancellation and unsupported environments fail explicitly, never as complete search coverage. Does not grant worker command execution.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(properties, Some(vec!["paths".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "file_search requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let argv = args.argv()?;
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                None,
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("file_search requires an environment".into())
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel("file_search requires local full-disk read access; use sandboxed read_file search selectors for known files".into()));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let env = std::env::vars()
                .filter(|(key, _)| key != "RIPGREP_CONFIG_PATH")
                .collect();
            // Resolve through the existing selected-environment owner. Passing a
            // bare executable to CreateProcess would also search an untrusted cwd.
            let lookup = codex_exec_server::LookupExecutableParams {
                name: "rg".into(),
                cwd: environment.cwd().clone(),
                sandbox: Some(
                    invocation
                        .step_context
                        .turn
                        .file_system_sandbox_context(None, environment.cwd()),
                ),
            };
            let executable = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("file search cancelled before lookup".into())),
                result = environment.environment.lookup_executable(lookup) => result
                    .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?
                    .path.ok_or_else(|| FunctionCallError::RespondToModel("configured ripgrep executable unavailable".into()))?,
            }.inferred_native_path_string();
            // Existing pipe owner contains descendants, drains exact split streams,
            // and retains failed cleanup custody on cancellation/drop. No new tasks
            // or locks outlive that owner, and no model-visible process is created.
            let mut process = codex_utils_pty::spawn_pipe_process_no_stdin(
                &executable,
                &argv,
                cwd.as_path(),
                &env,
                &None,
            )
            .await
            .map_err(|e| {
                FunctionCallError::RespondToModel(format!("file search launch failed: {e}"))
            })?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut capture_overflowed = false;
            let collect = async {
                let (mut out_open, mut err_open) = (true, true);
                while out_open || err_open {
                    let (packet, target, open) = tokio::select! {
                        packet = process.stdout_rx.recv(), if out_open => (packet, &mut stdout, &mut out_open),
                        packet = process.stderr_rx.recv(), if err_open => (packet, &mut stderr, &mut err_open),
                    };
                    if let Some(packet) = packet {
                        if target.len() + packet.len() > 8 * 1024 * 1024 {
                            let remaining = 8 * 1024 * 1024 - target.len();
                            target.extend_from_slice(&packet[..remaining]);
                            capture_overflowed = true;
                            return Err(
                                "file search exceeded exact capture; narrow paths or globs"
                                    .to_owned(),
                            );
                        }
                        target.extend(packet);
                    } else {
                        *open = false;
                    }
                }
                let code = (&mut process.exit_rx).await.map_err(|e| e.to_string())?;
                if !process.session.output_capture().complete() {
                    return Err("file search producer did not certify both stream ends".to_owned());
                }
                Ok(code)
            };
            let result = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => Err("file search cancelled".to_owned()),
                result = tokio::time::timeout(Duration::from_secs(60), collect) => result.unwrap_or_else(|_| Err("file search timed out".to_owned())),
            };
            // Confirm custody settlement even after a successful root exit; a PATH
            // shim must not leave a descendant behind this read-only operation.
            let cleanup_error = process.session.terminate().err().map(|error| error.to_string());
            let roots = args.paths.iter().map(|requested| {
                let resolved = environment.cwd().join(requested);
                match resolved {
                    Ok(path) => json!({"requested":requested,"resolved":path.inferred_native_path_string(),"error":null}),
                    Err(error) => json!({"requested":requested,"resolved":null,"error":error.to_string()}),
                }
            }).collect::<Vec<_>>();
            let environment_receipt = json!({"roots":[{"environment":{
                "environment_id":environment.environment_id,
                "cwd":environment.cwd(),"search_roots":roots}}]});
            if capture_overflowed {
                let mut captures = Vec::new();
                for (stream, bytes) in [("stdout", stdout), ("stderr", stderr)] {
                    if !bytes.is_empty() {
                        captures.push(retain_capture(&invocation, stream, bytes).await);
                    }
                }
                let mut failure = ToolFailure::new(
                    ToolFailureCode::Failed,
                    "file search exceeded exact capture; recover the retained prefix, then narrow paths or globs",
                ).with_completed_execution();
                failure.safe_next_actions = vec![
                    "Inspect retained captured prefixes; they do not establish complete search coverage. Narrow paths/globs or omit lines for further authorized acquisition; do not automatically replay this search.".into(),
                ];
                failure.details = Some(json!({
                    "search_backend":"native_file_search",
                    "capture_failure":"exact_capture_overflow",
                    "streams_complete":false,
                    "execution_state":if cleanup_error.is_none() { "exited" } else { "unknown" },
                    "process_exited":cleanup_error.as_ref().map(|_| serde_json::Value::Null).unwrap_or(json!(true)),
                    "cleanup_error":cleanup_error,
                    "environment_receipt":environment_receipt,
                    "coverage":{"complete":false,"lower_bound":true,"scope_exhausted":null,
                        "result_limit_hit":null,"read_errors":{"present":null},
                        "reasons":["exact_capture_incomplete"]},
                    "delivery":{"complete":false,"truncated":true,"exact_capture_complete":false},
                    "captures":captures,
                }));
                return Err(FunctionCallError::structured(failure));
            }
            if let Some(error) = cleanup_error {
                return Err(FunctionCallError::RespondToModel(format!("file search cleanup failed: {error}")));
            }
            let exit_code = result.map_err(FunctionCallError::RespondToModel)?;
            let stdout = String::from_utf8(stdout)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let stderr = String::from_utf8(stderr)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let output = JsonToolOutput::new(
                json!({"exit_code":exit_code,"stdout":stdout,"stderr":stderr,
                "search_backend":"native_file_search",
                "streams_complete":true,"execution_state":"exited","process_exited":true,
                "environment_receipt":environment_receipt}),
            );
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
            Ok(boxed_tool_output(output))
        })
    }
}

/// Retain exact bytes, including a possible split UTF-8/JSON record, through the
/// existing artifact owner. A complete retained prefix is not a complete search.
async fn retain_capture(invocation: &ToolInvocation, stream: &str, bytes: Vec<u8>) -> serde_json::Value {
    let canonical = codex_tools::CanonicalToolResult::bytes(bytes);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        invocation.step_context.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &canonical,
    ).await;
    let mut receipt = json!({"stream":stream,"captured_bytes":canonical.exact_bytes,
        "canonical_sha256":canonical.sha256,"capture_complete":false,
        "retention_status":"unavailable"});
    if let Some(id) = artifact.artifact_id().filter(|_| artifact.complete) {
        match invocation.session.try_register_tool_artifact_origin(
            id.clone(), invocation.call_id.clone(), canonical.exact_bytes, canonical.sha256.clone(),
        ).await {
            Ok(()) => {
                receipt["retention_status"] = json!("retained");
                receipt["artifact_id"] = json!(id);
                receipt["recovery"] = json!({"tool":"read_tool_output","arguments":{
                    "artifact_id":id,"selectors":[{"kind":"bytes","start":0,
                        "end":canonical.exact_bytes.min(4096)}]}});
            }
            Err(error) => receipt["error"] = json!(error.to_string()),
        }
    } else {
        receipt["error"] = json!(artifact.error);
    }
    receipt
}

impl CoreToolRuntime for FileSearchHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::ProcessTermination
    }
}
