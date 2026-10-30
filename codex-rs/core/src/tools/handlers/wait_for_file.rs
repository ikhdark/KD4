use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileSystemSandboxContext;
use codex_file_watcher::WaitOutcome;
use codex_file_watcher::WatchPath;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_TIMEOUT_MS: u64 = 300_000;
const BUSY_RECHECK_INITIAL: Duration = Duration::from_millis(25);
const BUSY_RECHECK_MAX: Duration = Duration::from_millis(500);

pub(crate) struct WaitForFileHandler;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Condition {
    Exists,
    Absent,
    #[default]
    Changed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
    environment_id: Option<String>,
    #[serde(default)]
    condition: Condition,
    timeout_ms: Option<u64>,
}

// Use the filesystem owner's opened-object revision, not size/mtime or the
// notification itself. A one-byte range is bounded even for very large files.
// Windows denies opening a directory or a delete-pending file; metadata tells
// an unsupported directory and a removed file apart from that denial.
async fn revision(
    filesystem: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
) -> io::Result<Option<String>> {
    match filesystem
        .read_file_range(path, 0, 1, None, Some(sandbox))
        .await
    {
        Ok(read) if !read.revision.is_empty() => Ok(Some(read.revision)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem returned no file revision",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            match filesystem.get_metadata(path, Some(sandbox)).await {
                Ok(metadata) if metadata.is_directory => Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "changed requires a regular file; use exists or absent for directories",
                )),
                Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                    Ok(None)
                }
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

async fn path_exists(
    filesystem: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
) -> io::Result<bool> {
    match filesystem.get_metadata(path, Some(sandbox)).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Windows sharing/lock violations (os errors 32/33). Sandbox helpers forward
/// only the message, so its suffix identifies them as well.
pub(super) fn sharing_violation(error: &io::Error) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let message = error.to_string();
    matches!(error.raw_os_error(), Some(32 | 33))
        || message.ends_with("(os error 32)")
        || message.ends_with("(os error 33)")
}

// Busy states hide the path's state without describing it: native patch
// capture, Windows sharing/lock violations, a revision change during the probe
// and, once the file was readable, the access denial Windows reports for a
// pending deletion.
fn transient(error: &io::Error, was_readable: bool) -> bool {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::ResourceBusy => true,
        io::ErrorKind::PermissionDenied => was_readable,
        _ => {
            sharing_violation(error)
                || error
                    .to_string()
                    .contains("changed during byte-range acquisition")
        }
    }
}

// Releasing a busy state may produce no notification, so recheck on a bounded
// backoff. The caller's absolute deadline and cancellation bound every attempt,
// so `last_busy` keeps an unresolved busy error for a timeout to report.
async fn observe<T, Fut>(
    mut probe: impl FnMut() -> Fut,
    is_transient: impl Fn(&io::Error) -> bool,
    last_busy: &Mutex<Option<String>>,
) -> io::Result<T>
where
    Fut: Future<Output = io::Result<T>>,
{
    let mut delay = BUSY_RECHECK_INITIAL;
    loop {
        match probe().await {
            Err(error) if is_transient(&error) => {
                *last_busy.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
            }
            result => {
                *last_busy.lock().unwrap_or_else(PoisonError::into_inner) = None;
                return result;
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(BUSY_RECHECK_MAX);
    }
}

impl ToolExecutor<ToolInvocation> for WaitForFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_for_file")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let condition = JsonSchema {
            enum_values: Some(vec![json!("exists"), json!("absent"), json!("changed")]),
            ..JsonSchema::string(Some(
                "Requested state; default changed. changed requires a regular file with revision support; a directory fails."
                    .into(),
            ))
        };
        let timeout = JsonSchema {
            minimum: Some(1.into()),
            maximum: Some(MAX_TIMEOUT_MS.into()),
            ..JsonSchema::integer(Some("Total wait budget in milliseconds, including environment startup and registration; default 30000, maximum 300000. Nested Code Mode deadlines may shorten it.".into()))
        };
        ToolSpec::Function(ResponsesApiTool {
            name: "wait_for_file".into(),
            description: "Wait for local filesystem state without polling or shell sleeps, using the shared filesystem watcher. Conditions: exists, absent, or changed (default). changed compares a regular file's existence/opened-object revision against an initial observation, not content equivalence; metadata-only changes may qualify. exists/absent check path metadata and also accept directories. Notifications, duplicates and overflow only trigger sandboxed state checks; they never establish success. Returns status matched, timed_out, deadline_exceeded or cancelled, plus elapsed_ms. A timeout whose latest check was still failing transiently (e.g. a sharing/lock violation, concurrent patch capture, pending deletion, or denial after a readable baseline) also returns that error as last_probe_error. Timeouts/cancellation do not prove the condition. The observed state can change after return. Remote environments are unsupported; do not substitute host paths.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Path, absolute or relative to the selected environment cwd; missing paths are supported.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Execution environment; omit for the selected default. Local native filesystem only.".into()))),
                ("condition".into(), condition),
                ("timeout_ms".into(), timeout),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let error = |message: String| FunctionCallError::RespondToModel(message);
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(error("wait_for_file requires function arguments".into()));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let timeout_ms = args.timeout_ms.unwrap_or(30_000);
            if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) || args.path.is_empty() {
                return Err(error(
                    "wait_for_file requires a nonempty path and timeout_ms from 1 to 300000".into(),
                ));
            }
            let started = Instant::now();
            let requested_deadline = started + Duration::from_millis(timeout_ms);
            let deadline =
                crate::unified_exec::nested_poll_bound(invocation.source.nested_deadline())
                    .map_or(requested_deadline, |bound| bound.min(requested_deadline));
            let last_busy = Mutex::new(None);
            let operation = async {
                let environment = wait_for_tool_environment(
                    &invocation.step_context.environments,
                    args.environment_id.as_deref(),
                    &invocation.cancellation_token,
                )
                .await?
                .ok_or_else(|| {
                    error("wait_for_file requires a selected execution environment".into())
                })?;
                let filesystem = environment.environment.get_filesystem();
                if environment.environment.is_remote() || !filesystem.uses_host_native_paths() {
                    return Err(error("wait_for_file requires a local native filesystem; remote watching is unavailable".into()));
                }
                let path = environment
                    .cwd()
                    .join(&args.path)
                    .map_err(|err| error(err.to_string()))?;
                let native = path.to_abs_path().map_err(|err| error(err.to_string()))?;
                let sandbox = invocation
                    .step_context
                    .turn
                    .file_system_sandbox_context(None, environment.cwd());
                // A busy file's baseline is its first readable observation;
                // changes completed while it was unreadable are not reported.
                let baseline = if matches!(args.condition, Condition::Changed) {
                    observe(
                        || revision(filesystem.as_ref(), &path, &sandbox),
                        |err| transient(err, false),
                        &last_busy,
                    )
                    .await
                    .map_err(|err| {
                        error(format!("unable to observe initial file revision: {err}"))
                    })?
                } else {
                    None
                };
                codex_file_watcher::wait_until(
                    vec![WatchPath {
                        path: native.to_path_buf(),
                        recursive: false,
                    }],
                    deadline,
                    invocation.cancellation_token.cancelled(),
                    || async {
                        match args.condition {
                            Condition::Changed => {
                                let current = observe(
                                    || revision(filesystem.as_ref(), &path, &sandbox),
                                    |err| transient(err, baseline.is_some()),
                                    &last_busy,
                                )
                                .await?;
                                Ok(current != baseline)
                            }
                            Condition::Exists | Condition::Absent => {
                                let exists = observe(
                                    || path_exists(filesystem.as_ref(), &path, &sandbox),
                                    |err| transient(err, false),
                                    &last_busy,
                                )
                                .await?;
                                Ok(exists == matches!(args.condition, Condition::Exists))
                            }
                        }
                    },
                )
                .await
                .map_err(|err| {
                    error(format!(
                        "unable to wait for {}: {err}",
                        path.inferred_native_path_string()
                    ))
                })
            };
            let outcome = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => WaitOutcome::Cancelled,
                _ = tokio::time::sleep_until(deadline) => WaitOutcome::TimedOut,
                result = operation => result?,
            };
            let outcome = if invocation.cancellation_token.is_cancelled() {
                WaitOutcome::Cancelled
            } else if Instant::now() >= deadline {
                WaitOutcome::TimedOut
            } else {
                outcome
            };
            let status = match outcome {
                WaitOutcome::Satisfied => "matched",
                WaitOutcome::Cancelled => "cancelled",
                WaitOutcome::TimedOut if deadline < requested_deadline => "deadline_exceeded",
                WaitOutcome::TimedOut => "timed_out",
            };
            let mut value = json!({
                "path": args.path,
                "status": status,
                "elapsed_ms": started.elapsed().as_millis(),
            });
            // A bare expiry would hide why the latest check never resolved.
            let last_probe_error = last_busy
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if outcome == WaitOutcome::TimedOut
                && let Some(message) = last_probe_error
            {
                value["last_probe_error"] = json!(message);
            }
            Ok(boxed_tool_output(JsonToolOutput::new(value)))
        })
    }
}

impl CoreToolRuntime for WaitForFileHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
