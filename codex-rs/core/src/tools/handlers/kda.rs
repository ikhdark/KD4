use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const TOOL_NAME: &str = "kda";
const KDA_PROGRAM: &str = "cargo-kda.exe";
const MAX_ARGUMENTS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KdaArgs {
    #[serde(default)]
    args: Vec<String>,
}

pub struct KdaHandler {
    cwd: PathBuf,
}

impl KdaHandler {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl ToolExecutor<ToolInvocation> for KdaHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_kda_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                cancellation_token,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "kda handler received unsupported payload".to_string(),
                ));
            };
            let args: KdaArgs = parse_arguments(&arguments)?;
            validate_args(&args.args)?;
            let output = run_kda(
                Path::new(KDA_PROGRAM),
                &self.cwd,
                &args.args,
                &cancellation_token,
            )
            .await?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for KdaHandler {}

fn create_kda_tool() -> ToolSpec {
    let args = JsonSchema {
        max_items: Some(MAX_ARGUMENTS as u64),
        ..JsonSchema::array(
            JsonSchema::string(None),
            Some(
                "Arguments to pass after `cargo kda`. Omit for the default gate. Output is always report-json, workspace execution is disabled, and write modes are rejected."
                    .to_string(),
            ),
        )
    };
    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Run the installed KDA Rust quality gate in the active local workspace. Exit code 1 is a successful analysis containing deny findings; exit codes 2 and 3 are failures."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([("args".to_string(), args)]),
            /*required*/ None,
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

fn validate_args(args: &[String]) -> Result<(), FunctionCallError> {
    if args.len() > MAX_ARGUMENTS {
        return Err(FunctionCallError::RespondToModel(format!(
            "kda accepts at most {MAX_ARGUMENTS} arguments"
        )));
    }

    for arg in args {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if flag == "--format" {
            return Err(FunctionCallError::RespondToModel(
                "kda output format is fixed to report-json".to_string(),
            ));
        }
        if flag == "--apply" || flag.starts_with("--write-") {
            return Err(FunctionCallError::RespondToModel(format!(
                "kda runtime tool does not permit write option `{flag}`"
            )));
        }
    }

    Ok(())
}

async fn run_kda(
    program: &Path,
    cwd: &Path,
    args: &[String],
    cancellation_token: &CancellationToken,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .args(["--format", "report-json", "--no-workspace-exec"])
        .current_dir(cwd)
        .kill_on_drop(true);

    let output = tokio::select! {
        _ = cancellation_token.cancelled() => {
            return Err(FunctionCallError::RespondToModel(
                "KDA analysis was cancelled".to_string(),
            ));
        }
        output = command.output() => output.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to start `{}`: {err}",
                program.display()
            ))
        })?,
    };

    let exit_code = output.status.code();
    let completed_analysis = matches!(exit_code, Some(0 | 1));
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let body = if completed_analysis && !stdout.is_empty() {
        stdout
    } else {
        let code = exit_code.map_or_else(|| "terminated".to_string(), |code| code.to_string());
        match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("cargo-kda exited with code {code}\n{stdout}\n{stderr}"),
            (false, true) => format!("cargo-kda exited with code {code}\n{stdout}"),
            (true, false) => format!("cargo-kda exited with code {code}\n{stderr}"),
            (true, true) => format!("cargo-kda exited with code {code} and produced no output"),
        }
    };

    Ok(FunctionToolOutput::from_text(
        body,
        Some(completed_analysis),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_runtime_reject_write_modes() {
        let ToolSpec::Function(tool) = create_kda_tool() else {
            panic!("kda must remain a function tool");
        };
        let schema = serde_json::to_value(&tool.parameters).expect("serialize kda schema");
        let validator = jsonschema::validator_for(&schema).expect("compile kda schema");
        assert!(validator.is_valid(&serde_json::json!({})));
        assert!(validator.is_valid(&serde_json::json!({
            "args": ["impact", "--base", "HEAD~1"]
        })));
        assert!(validate_args(&["--apply".to_string()]).is_err());
        assert!(validate_args(&["--write-baseline=baseline.json".to_string()]).is_err());
        assert!(validate_args(&["--format".to_string(), "human".to_string()]).is_err());
    }

    #[tokio::test]
    async fn exit_one_is_a_successful_report_json_analysis() {
        let dir = tempfile::tempdir().expect("temporary KDA runner directory");
        let runner = dir.path().join("cargo-kda.cmd");
        std::fs::write(
            &runner,
            "@echo off\r\nif not \"%~1\"==\"impact\" exit /b 3\r\nif not \"%~2\"==\"--base\" exit /b 3\r\nif not \"%~3\"==\"HEAD~1\" exit /b 3\r\nif not \"%~4\"==\"--format\" exit /b 3\r\nif not \"%~5\"==\"report-json\" exit /b 3\r\nif not \"%~6\"==\"--no-workspace-exec\" exit /b 3\r\necho {\"schema_version\":1,\"gate_denied\":true}\r\nexit /b 1\r\n",
        )
        .expect("write fake KDA runner");

        let output = run_kda(
            &runner,
            dir.path(),
            &[
                "impact".to_string(),
                "--base".to_string(),
                "HEAD~1".to_string(),
            ],
            &CancellationToken::new(),
        )
        .await
        .expect("run fake KDA analysis");

        assert_eq!(output.success, Some(true));
        assert_eq!(
            output.into_text(),
            r#"{"schema_version":1,"gate_denied":true}"#
        );
    }
}
