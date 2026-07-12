use crate::exec::ExecCapturePolicy;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::session::step_context::StepContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::shell::RunExecLikeArgs;
use crate::tools::handlers::shell::run_exec_like;
use crate::tools::handlers::verify_local_spec::VERIFY_LOCAL_TOOL_NAME;
use crate::tools::handlers::verify_local_spec::VerifyLocalToolOptions;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

const VERIFY_LOCAL_JSON_SCHEMA_VERSION: u64 = 1;
const VERIFY_LOCAL_JSON_PRODUCER: &str = "kd4.verify_local";

#[derive(Debug)]
struct VerifyLocalArgs {
    mode_flag: &'static str,
    changed: Vec<String>,
    staged: bool,
    scope_current: bool,
    no_cache: bool,
    json: bool,
    environment_id: Option<String>,
}

#[derive(Debug)]
struct VerifyLocalRun {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    json: Option<Value>,
    verdict_text: Option<String>,
    tool_success: bool,
    guidance: Option<&'static str>,
    scope: Option<VerifyLocalScope>,
}

impl VerifyLocalRun {
    fn is_proof_bearing(&self) -> bool {
        self.tool_success
            && self.verdict_text.as_deref() == Some("VERIFIED")
            && self
                .json
                .as_ref()
                .is_some_and(self::is_versioned_verify_local_json)
            && self.scope.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyLocalScope {
    source: String,
    active_files: Vec<PathBuf>,
    ignored_dirty_files: Vec<PathBuf>,
    stale_reasons: Vec<String>,
}

pub struct VerifyLocalHandler {
    options: VerifyLocalToolOptions,
}

impl VerifyLocalHandler {
    pub(crate) fn for_verify_local_environment_id(include_environment_id: bool) -> Self {
        Self {
            options: VerifyLocalToolOptions::with_verify_local_environment_id(
                include_environment_id,
            ),
        }
    }
}

impl ToolExecutor<ToolInvocation> for VerifyLocalHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(VERIFY_LOCAL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        (crate::tools::handlers::verify_local_spec::VERIFY_LOCAL_TOOL_BUILDER)(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for VerifyLocalHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

impl VerifyLocalHandler {
    pub(crate) fn is_available_for_step(step_context: &StepContext) -> bool {
        step_context
            .environments
            .turn_environments
            .iter()
            .any(|environment| {
                !environment.environment.is_remote()
                    && find_verify_local_repo_root(environment.cwd()).is_some()
            })
    }

    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            payload,
            tracker,
            call_id,
            ..
        } = invocation;
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "verify_local handler received unsupported payload".to_string(),
                ));
            }
        };

        let args = parse_verify_local_arguments(&arguments)?;
        let environment =
            resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?
                .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "verify_local requires a selected local turn environment".to_string(),
                )
            })?;
        if environment.environment.is_remote() {
            return Err(FunctionCallError::RespondToModel(
                "verify_local is available only in the selected local environment".to_string(),
            ));
        }
        let repo_root = find_verify_local_repo_root(environment.cwd()).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "verify_local is unavailable: expected scripts/verify_local.py and justfile in this repo"
                    .to_string(),
            )
        })?;

        let argv = build_verify_local_argv(&args);
        let raw_json = args.json;
        let validation_command = argv.clone();
        let validation_tracker = tracker.clone();
        let validation_environment_id = environment.environment_id.clone();
        let (isolation, isolated_codex_home, isolated_sqlite_home) =
            create_isolated_validation_state()?;

        let mut env = create_env(
            &turn.config.permissions.shell_environment_policy,
            Some(session.thread_id),
        );
        let active_permission_profile = turn.config.permissions.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());
        env.insert(
            "CODEX_HOME".to_string(),
            isolated_codex_home.to_string_lossy().into_owned(),
        );
        env.insert(
            "CODEX_SQLITE_HOME".to_string(),
            isolated_sqlite_home.to_string_lossy().into_owned(),
        );

        let cwd = AbsolutePathBuf::from_absolute_path(repo_root).map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid verify_local repo root: {err}"))
        })?;
        let hook_command = codex_shell_command::parse_command::shlex_join(&argv);
        let exec_params = ExecParams {
            command: argv.clone(),
            cwd,
            expiration: None.into(),
            capture_policy: ExecCapturePolicy::ShellTool,
            env,
            network: turn.network.clone(),
            network_environment_id: Some(environment.environment_id.clone()),
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level: turn.windows_sandbox_level,
            windows_sandbox_private_desktop: turn
                .config
                .permissions
                .windows_sandbox_private_desktop,
            justification: None,
            arg0: None,
        };

        // Keep the temporary CODEX_HOME/SQLite directories alive until the
        // orchestrated command and cancellation cleanup have fully completed.
        let _isolation = isolation;
        let output = run_exec_like(RunExecLikeArgs {
            tool_name: self.tool_name(),
            exec_params,
            cancellation_token,
            hook_command,
            safety_command: argv,
            shell_type: None,
            additional_permissions: None,
            prefix_rule: None,
            session,
            turn,
            turn_environment: environment.clone(),
            tracker,
            call_id,
            shell_runtime_backend: ShellRuntimeBackend::ShellCommandClassic,
            track_validation_freshness: false,
            attempt_key: None,
            repair_notice: None,
        })
        .await?;
        let (output, run) = finalize_verify_local_output(output, raw_json);
        if run.is_proof_bearing()
            && let Some(scope) = &run.scope
        {
            let mut tracker = validation_tracker.lock().await;
            crate::turn_diff_tracker::TurnDiffTracker::record_verified_validation(
                &mut tracker,
                validation_command,
                &validation_environment_id,
                &scope.active_files,
                /*clear_unknown_mutation*/ false,
            );
        }
        Ok(boxed_tool_output(output))
    }
}

fn parse_verify_local_arguments(arguments: &str) -> Result<VerifyLocalArgs, FunctionCallError> {
    let value: Value = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse verify_local arguments: {err}"))
    })?;
    let Some(object) = value.as_object() else {
        return Err(FunctionCallError::RespondToModel(
            "verify_local arguments must be a JSON object".to_string(),
        ));
    };
    let allowed = [
        "mode",
        "changed",
        "staged",
        "scope_current",
        "no_cache",
        "json",
        "environment_id",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let broadening = [
        "all_dirty",
        "allow_workspace",
        "related",
        "related_tests",
        "isolated",
        "baseline",
        "retry_flakes",
        "cache_readonly",
        "regen",
        "scope_start",
        "scope_add",
        "scope_reset",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    for key in object.keys() {
        if broadening.contains(key.as_str()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown field `{key}`; broadening or mutating flags are human CLI-only. Narrow validation with `changed`, `staged`, or `scope_current` instead."
            )));
        }
        if !allowed.contains(key.as_str()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown field `{key}`; verify_local only accepts read-only narrowing fields. Narrow validation with `changed`, `staged`, or `scope_current` instead."
            )));
        }
    }

    let mode_flag = match object.get("mode").and_then(Value::as_str) {
        Some("plan") => "--plan",
        Some("fast") => "--fast",
        Some("final") => "--final",
        Some(other) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "failed to parse verify_local arguments: unsupported mode `{other}`"
            )));
        }
        None => {
            return Err(FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: missing string field `mode`".to_string(),
            ));
        }
    };
    let changed = object
        .get("changed")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: missing array field `changed`".to_string(),
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "failed to parse verify_local arguments: `changed` must contain only strings"
                        .to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let read_bool = |name: &str| -> Result<bool, FunctionCallError> {
        object.get(name).and_then(Value::as_bool).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse verify_local arguments: missing boolean field `{name}`"
            ))
        })
    };
    let environment_id = match object.get("environment_id") {
        Some(Value::String(environment_id)) => Some(environment_id.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: `environment_id` must be a string or null"
                    .to_string(),
            ));
        }
    };

    Ok(VerifyLocalArgs {
        mode_flag,
        changed,
        staged: read_bool("staged")?,
        scope_current: read_bool("scope_current")?,
        no_cache: read_bool("no_cache")?,
        json: read_bool("json")?,
        environment_id,
    })
}

fn build_verify_local_argv(args: &VerifyLocalArgs) -> Vec<String> {
    let mut argv = vec![
        "just".to_string(),
        "verify-local".to_string(),
        args.mode_flag.to_string(),
        "--json".to_string(),
    ];
    for path in &args.changed {
        argv.push(format!("--changed={path}"));
    }
    if args.staged {
        argv.push("--staged".to_string());
    }
    if args.scope_current {
        argv.push("--scope".to_string());
        argv.push("current".to_string());
    }
    if args.no_cache {
        argv.push("--no-cache".to_string());
    }
    argv
}

fn find_verify_local_repo_root(cwd: &PathUri) -> Option<PathBuf> {
    let cwd = cwd.to_abs_path().ok()?;
    for candidate in cwd.as_path().ancestors() {
        if candidate.join("scripts").join("verify_local.py").is_file()
            && candidate.join("justfile").is_file()
        {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn create_isolated_validation_state()
-> Result<(tempfile::TempDir, PathBuf, PathBuf), FunctionCallError> {
    let isolation = tempfile::Builder::new()
        .prefix("codex-verify-local-")
        .tempdir()
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to isolate verify_local state: {err}"
            ))
        })?;
    let codex_home = isolation.path().join("codex-home");
    let sqlite_home = isolation.path().join("sqlite-home");
    std::fs::create_dir_all(&codex_home)
        .and_then(|_| std::fs::create_dir_all(&sqlite_home))
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to create isolated verify_local state: {err}"
            ))
        })?;
    Ok((isolation, codex_home, sqlite_home))
}

fn parse_verify_local_run(
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
) -> VerifyLocalRun {
    let json = parse_verify_local_json(&stdout);
    let has_versioned_json = json
        .as_ref()
        .is_some_and(self::is_versioned_verify_local_json);
    let verdict_text = if has_versioned_json {
        json.as_ref()
            .and_then(|value| value.get("verdict"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        parse_text_verdict(&stdout, &stderr)
    };
    let exit_code = exit_code.or_else(|| parse_formatted_exit_code(&stdout));
    let (semantic_success, guidance) = if json.is_some() && !has_versioned_json {
        (
            false,
            Some(
                "The verifier returned an unsupported JSON contract; expected schema_version 1 from kd4.verify_local.",
            ),
        )
    } else {
        match verdict_text.as_deref() {
            Some("VERIFIED") => (true, None),
            Some("VERIFIED (no proof needed)") => (
                true,
                Some(
                    "VERIFIED (no proof needed) completed cleanly but does not count as proof-bearing validation evidence.",
                ),
            ),
            Some("PLANNED") => (
                true,
                Some(
                    "PLANNED returned the verifier plan only; run fast or final mode for proof-bearing validation.",
                ),
            ),
            Some("NEEDS_SCOPE") => (
                false,
                Some(
                    "NEEDS_SCOPE: narrow validation with changed, staged, or scope_current, then rerun verify_local.",
                ),
            ),
            Some("NEEDS_REGEN") => (
                false,
                Some(
                    "NEEDS_REGEN: regeneration is mutating and CLI-only, so this is an autonomous blocker.",
                ),
            ),
            Some(_) | None => (
                false,
                Some(
                    "The verifier did not produce proof-bearing validation; fix the issue or report the blocker.",
                ),
            ),
        }
    };
    let tool_success = semantic_success && exit_code == Some(0);
    let scope = json
        .as_ref()
        .filter(|_| has_versioned_json)
        .and_then(self::parse_json_scope_value);
    VerifyLocalRun {
        exit_code,
        stdout,
        stderr,
        json,
        verdict_text,
        tool_success,
        guidance,
        scope,
    }
}

fn is_versioned_verify_local_json(value: &Value) -> bool {
    value.get("schema_version").and_then(Value::as_u64)
        == Some(self::VERIFY_LOCAL_JSON_SCHEMA_VERSION)
        && value.get("producer").and_then(Value::as_str) == Some(self::VERIFY_LOCAL_JSON_PRODUCER)
}

fn parse_verify_local_json(stdout: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        return Some(value);
    }

    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(Value::is_object)
        })
        .or_else(|| {
            stdout
                .char_indices()
                .filter_map(|(idx, ch)| (ch == '{').then_some(idx))
                .filter_map(|idx| parse_json_object_at(stdout, idx))
                .filter(|value| value.get("verdict").is_some())
                .last()
        })
}

fn parse_json_object_at(stdout: &str, start: usize) -> Option<Value> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in stdout[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return serde_json::from_str::<Value>(&stdout[start..end]).ok();
                }
            }
            _ => {}
        }
    }

    None
}

fn parse_json_scope_value(value: &Value) -> Option<VerifyLocalScope> {
    let scope = value.get("scope")?;
    if scope.is_null() {
        return None;
    }
    let source = scope
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(VerifyLocalScope {
        source,
        active_files: parse_path_array(scope.get("active_files")),
        ignored_dirty_files: parse_path_array(scope.get("ignored_dirty_files")),
        stale_reasons: parse_string_array(scope.get("stale_reasons")),
    })
}

fn parse_path_array(value: Option<&Value>) -> Vec<PathBuf> {
    parse_string_array(value)
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn parse_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn parse_text_verdict(stdout: &str, stderr: &str) -> Option<String> {
    stdout
        .lines()
        .chain(stderr.lines())
        .find_map(|line| line.trim().strip_prefix("Verdict:").map(str::trim))
        .map(str::to_string)
}

fn parse_formatted_exit_code(output: &str) -> Option<i32> {
    ["Process exited with code ", "Exit code: "]
        .into_iter()
        .find_map(|prefix| {
            output.lines().find_map(|line| {
                line.trim()
                    .strip_prefix(prefix)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
        })
}

fn finalize_verify_local_output(
    output: FunctionToolOutput,
    raw_json: bool,
) -> (FunctionToolOutput, VerifyLocalRun) {
    let original_post_tool_use_response = output.post_tool_use_response.clone();
    let run = parse_verify_local_run(output.into_text(), String::new(), None);
    let mut transformed = FunctionToolOutput::from_text(
        render_verify_local_output(&run, raw_json),
        Some(run.tool_success),
    );
    transformed.post_tool_use_response = run.json.clone().or(original_post_tool_use_response);
    (transformed, run)
}

fn render_verify_local_output(run: &VerifyLocalRun, raw_json: bool) -> String {
    if raw_json {
        if let Some(value) = &run.json {
            return serde_json::to_string_pretty(value).unwrap_or_else(|_| render_raw_output(run));
        }
        return render_raw_output(run);
    }

    let exit_code = run
        .exit_code
        .map_or_else(|| "unknown".to_string(), |code| code.to_string());
    let verdict = run.verdict_text.as_deref().unwrap_or("UNKNOWN");
    let mut text = format!("Verdict: {verdict}\nExit code: {exit_code}");
    if let Some(scope) = &run.scope {
        text.push_str("\nScope: ");
        text.push_str(&scope.source);
        if !scope.active_files.is_empty() {
            text.push_str(&format!(" ({} active file(s))", scope.active_files.len()));
        }
    }
    if let Some(guidance) = run.guidance {
        text.push_str("\nGuidance: ");
        text.push_str(guidance);
    }
    if !run.stderr.trim().is_empty() {
        text.push_str("\n\nStderr:\n");
        text.push_str(run.stderr.trim());
    }
    if run.verdict_text.as_deref() != Some("VERIFIED") && !run.stdout.trim().is_empty() {
        text.push_str("\n\nStdout:\n");
        text.push_str(run.stdout.trim());
    }
    text
}

fn render_raw_output(run: &VerifyLocalRun) -> String {
    let exit_code = run
        .exit_code
        .map_or_else(|| "unknown".to_string(), |code| code.to_string());
    let mut text = format!("Exit code: {exit_code}\nStdout:\n{}", run.stdout.trim());
    if !run.stderr.trim().is_empty() {
        text.push_str("\n\nStderr:\n");
        text.push_str(run.stderr.trim());
    }
    if let Some(guidance) = run.guidance {
        text.push_str("\n\nGuidance: ");
        text.push_str(guidance);
    }
    text
}

#[cfg(test)]
#[path = "verify_local_tests.rs"]
mod tests;
