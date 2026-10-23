//! Bridges Apps SDK-style `openai/fileParams` metadata into Codex's MCP flow.
//!
//! Strategy:
//! - Inspect `_meta["openai/fileParams"]` to discover which tool arguments are
//!   file inputs.
//! - At tool execution time, read those files from the current sampling step's
//!   primary environment and upload them to OpenAI file storage,
//!   and rewrite only the declared arguments into the provided-file payload
//!   shape expected by the downstream Apps tool.
//!
//! The model-facing local-path schema is owned by `codex-mcp` alongside MCP tool inventory, so this
//! module only handles uploading the files and rewriting the execution-time arguments.

use crate::session::session::Session;
use crate::session::step_context::StepContext;
#[cfg(test)]
use crate::session::turn_context::TurnContext;
use codex_api::OPENAI_FILE_UPLOAD_LIMIT_BYTES;
use codex_api::delete_openai_file_with_pool;
use codex_api::openai_file_http_client_pool;
use codex_api::upload_openai_file_with_pool;
#[cfg(test)]
use codex_login::CodexAuth;
use codex_utils_path_uri::PathConvention;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::Value as JsonValue;

const MAX_CONCURRENT_OPENAI_FILE_UPLOADS: usize = 4;

struct StagedOpenAiFile {
    field_name: String,
    index: Option<usize>,
    file_path: String,
    file_name: String,
    contents: Vec<u8>,
}

enum StagedOpenAiArgument {
    Single(StagedOpenAiFile),
    Array(Vec<StagedOpenAiFile>),
}

#[derive(Default)]
struct OpenAiFileStagingBudget {
    staged_bytes: u64,
}

impl OpenAiFileStagingBudget {
    fn remaining_bytes(&self) -> u64 {
        OPENAI_FILE_UPLOAD_LIMIT_BYTES.saturating_sub(self.staged_bytes)
    }

    fn record_file(&mut self, file_size_bytes: usize) -> Result<(), String> {
        let file_size_bytes = u64::try_from(file_size_bytes).map_err(|error| error.to_string())?;
        let staged_bytes = self
            .staged_bytes
            .checked_add(file_size_bytes)
            .ok_or_else(|| "total staged file size overflowed".to_string())?;
        if staged_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES {
            return Err(format!(
                "total staged file size exceeds the limit of {OPENAI_FILE_UPLOAD_LIMIT_BYTES} bytes"
            ));
        }
        self.staged_bytes = staged_bytes;
        Ok(())
    }
}

pub(crate) struct PreparedOpenAiArguments {
    pub(crate) arguments: Option<JsonValue>,
    cleanup: Option<OpenAiUploadCleanup>,
}

impl PreparedOpenAiArguments {
    pub(crate) fn dispatched(mut self) -> Option<JsonValue> {
        if let Some(mut cleanup) = self.cleanup.take() {
            cleanup.file_ids.clear();
        }
        self.arguments.take()
    }
}

struct OpenAiUploadCleanup {
    tasks: tokio_util::task::TaskTracker,
    base_url: String,
    auth: codex_api::SharedAuthProvider,
    clients: codex_http_client::RouteAwareClientPool,
    file_ids: Vec<String>,
}

impl OpenAiUploadCleanup {
    async fn rollback(&mut self) {
        // Remote IDs are independent and deletion is idempotent. Keep each ID
        // in the guard until its own delete is confirmed, even if this waiter
        // is dropped; Drop can then retry all unconfirmed work safely.
        let pending = self.file_ids.iter().rev().cloned().collect::<Vec<_>>();
        let base_url = &self.base_url;
        let auth = &self.auth;
        let clients = &self.clients;
        let deletes = futures::stream::iter(pending.into_iter().map(|file_id| async move {
            let result =
                delete_openai_file_with_pool(base_url, auth.as_ref(), clients, &file_id).await;
            (file_id, result)
        }))
        .buffer_unordered(MAX_CONCURRENT_OPENAI_FILE_UPLOADS);
        tokio::pin!(deletes);
        let mut deleted = std::collections::HashSet::new();
        while let Some((file_id, result)) = deletes.next().await {
            match result {
                Ok(()) => {
                    deleted.insert(file_id);
                }
                Err(error) => {
                    tracing::warn!(%error, "uploaded file cleanup failed; retaining cleanup ownership")
                }
            }
        }
        self.file_ids.retain(|file_id| !deleted.contains(file_id));
    }
}

impl Drop for OpenAiUploadCleanup {
    fn drop(&mut self) {
        if self.file_ids.is_empty() {
            return;
        }
        let mut owner = Self {
            tasks: self.tasks.clone(),
            base_url: self.base_url.clone(),
            auth: self.auth.clone(),
            clients: self.clients.clone(),
            file_ids: std::mem::take(&mut self.file_ids),
        };
        self.tasks.spawn(async move {
            // Each HTTP request already has a deadline. Keep retries bounded;
            // surface unresolved cleanup rather than silently treating it as done.
            for _ in 0..3 {
                owner.rollback().await;
                if owner.file_ids.is_empty() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            tracing::warn!(
                remaining = owner.file_ids.len(),
                "uploaded file cleanup remains unconfirmed"
            );
            owner.file_ids.clear();
        });
    }
}

pub(crate) async fn prepare_mcp_tool_arguments_for_openai_files(
    sess: &Session,
    step_context: &StepContext,
    arguments_value: Option<JsonValue>,
    openai_file_input_params: Option<&[String]>,
) -> Result<PreparedOpenAiArguments, String> {
    let passthrough = |arguments| PreparedOpenAiArguments {
        arguments,
        cleanup: None,
    };
    let Some(params) = openai_file_input_params else {
        return Ok(passthrough(arguments_value));
    };
    let Some(arguments) = arguments_value.as_ref().and_then(JsonValue::as_object) else {
        return Ok(passthrough(arguments_value));
    };
    let requires_upload = params
        .iter()
        .filter_map(|key| arguments.get(key))
        .any(|value| {
            value.is_string()
                || value.as_array().is_some_and(|values| {
                    !values.is_empty() && values.iter().all(JsonValue::is_string)
                })
        });
    if !requires_upload {
        return Ok(passthrough(arguments_value));
    }
    // Authentication failure must not first stage local file contents.
    let auth = sess.services.auth_manager.auth().await;
    let Some(auth) = auth.as_ref().filter(|auth| auth.uses_codex_backend()) else {
        return Err("ChatGPT auth is required to upload files for Codex Apps tools".to_string());
    };
    let mut staged_arguments = Vec::new();
    let mut staging_budget = OpenAiFileStagingBudget::default();
    for field_name in params {
        if let Some(value) = arguments.get(field_name)
            && let Some(staged) = stage_argument_value_for_openai_files(
                step_context,
                field_name,
                value,
                &mut staging_budget,
            )
            .await?
        {
            staged_arguments.push((field_name.clone(), staged));
        }
    }
    let mut owner = OpenAiUploadCleanup {
        tasks: sess.terminal_tasks.clone(),
        base_url: step_context.turn.config.chatgpt_base_url.clone(),
        auth: codex_model_provider::auth_provider_from_auth(auth),
        clients: openai_file_http_client_pool(&step_context.turn.config.http_client_factory()),
        file_ids: Vec::new(),
    };
    let mut rewritten_arguments = arguments.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    sess.terminal_tasks.spawn(async move {
        let result = async {
            let mut fields = Vec::new();
            let mut pending = Vec::new();
            for (field_name, staged_argument) in staged_arguments {
                let (files, is_array) = match staged_argument {
                    StagedOpenAiArgument::Single(file) => (vec![file], false),
                    StagedOpenAiArgument::Array(files) => (files, true),
                };
                let field_index = fields.len();
                let values = vec![JsonValue::Null; files.len()];
                fields.push((field_name, is_array, values));
                pending.extend(
                    files
                        .into_iter()
                        .enumerate()
                        .map(|(index, file)| (field_index, index, file)),
                );
            }
            let mut pending = pending.into_iter();
            let mut uploads = FuturesUnordered::new();
            let mut failure = None;
            loop {
                if tx.is_closed() && failure.is_none() {
                    failure = Some("file preparation cancelled before MCP dispatch".to_string());
                }
                while failure.is_none() && uploads.len() < MAX_CONCURRENT_OPENAI_FILE_UPLOADS {
                    let Some((field_index, index, staged)) = pending.next() else {
                        break;
                    };
                    let base_url = owner.base_url.clone();
                    let auth = owner.auth.clone();
                    let clients = owner.clients.clone();
                    uploads.push(async move {
                        let result =
                            upload_staged_openai_file(&base_url, auth.as_ref(), &clients, staged)
                                .await;
                        (field_index, index, result)
                    });
                }
                let Some((field_index, index, result)) = uploads.next().await else {
                    break;
                };
                match result {
                    Ok((value, file_id)) => {
                        owner.file_ids.push(file_id);
                        fields[field_index].2[index] = value;
                    }
                    Err((error, cleanup_file_id)) => {
                        if let Some(file_id) = cleanup_file_id {
                            owner.file_ids.push(file_id);
                        }
                        match &mut failure {
                            Some(message) => {
                                message.push('\n');
                                message.push_str(&error);
                            }
                            None => failure = Some(error),
                        }
                    }
                }
                // Never drop an in-flight allocation/upload on error or caller
                // cancellation. Drain it into this session-owned rollback guard.
            }
            if let Some(error) = failure {
                return Err(error);
            }
            for (field_name, is_array, mut values) in fields {
                let value = if is_array {
                    JsonValue::Array(values)
                } else {
                    values.remove(0)
                };
                rewritten_arguments.insert(field_name, value);
            }
            Ok::<_, String>(Some(JsonValue::Object(rewritten_arguments)))
        }
        .await;
        if result.is_err() {
            owner.rollback().await;
        }
        let result = result.map(|arguments| PreparedOpenAiArguments {
            arguments,
            cleanup: Some(owner),
        });
        // A cancelled receiver drops the prepared guard, which owns rollback.
        let _ = tx.send(result);
    });
    rx.await
        .map_err(|error| format!("file preparation worker failed: {error}"))?
}

#[cfg(test)]
async fn rewrite_mcp_tool_arguments_for_openai_files(
    sess: &Session,
    step_context: &StepContext,
    arguments: Option<JsonValue>,
    params: Option<&[String]>,
) -> Result<Option<JsonValue>, String> {
    prepare_mcp_tool_arguments_for_openai_files(sess, step_context, arguments, params)
        .await
        .map(PreparedOpenAiArguments::dispatched)
}

async fn stage_argument_value_for_openai_files(
    step_context: &StepContext,
    field_name: &str,
    value: &JsonValue,
    staging_budget: &mut OpenAiFileStagingBudget,
) -> Result<Option<StagedOpenAiArgument>, String> {
    match value {
        JsonValue::String(file_path) => {
            let staged = stage_openai_file(
                step_context,
                field_name,
                /*index*/ None,
                file_path,
                staging_budget.remaining_bytes(),
            )
            .await?;
            staging_budget.record_file(staged.contents.len())?;
            Ok(Some(StagedOpenAiArgument::Single(staged)))
        }
        JsonValue::Array(values) => {
            let Some(file_paths) = values
                .iter()
                .map(JsonValue::as_str)
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(None);
            };
            let mut staged_files = Vec::with_capacity(file_paths.len());
            for (index, file_path) in file_paths.into_iter().enumerate() {
                let staged = stage_openai_file(
                    step_context,
                    field_name,
                    Some(index),
                    file_path,
                    staging_budget.remaining_bytes(),
                )
                .await?;
                staging_budget.record_file(staged.contents.len())?;
                staged_files.push(staged);
            }
            Ok(Some(StagedOpenAiArgument::Array(staged_files)))
        }
        _ => Ok(None),
    }
}

async fn stage_openai_file(
    step_context: &StepContext,
    field_name: &str,
    index: Option<usize>,
    file_path: &str,
    remaining_upload_bytes: u64,
) -> Result<StagedOpenAiFile, String> {
    let contextualize_error = |error: String| match index {
        Some(index) => {
            format!("failed to upload `{file_path}` for `{field_name}[{index}]`: {error}")
        }
        None => format!("failed to upload `{file_path}` for `{field_name}`: {error}"),
    };
    let Some(turn_environment) = step_context.environments.primary() else {
        return Err(contextualize_error(
            "no primary sampling-step environment is available".to_string(),
        ));
    };
    validate_relative_file_path(turn_environment.cwd(), file_path).map_err(contextualize_error)?;
    let path_uri = turn_environment
        .cwd()
        .join(file_path)
        .map_err(|error| contextualize_error(error.to_string()))?;
    if !path_uri.starts_with(turn_environment.cwd()) {
        return Err(contextualize_error(
            "file path resolves outside the selected environment working directory".to_string(),
        ));
    }
    let fs = turn_environment.environment.get_filesystem();
    let sandbox = step_context
        .turn
        .file_system_sandbox_context(/*additional_permissions*/ None, turn_environment.cwd());
    let contents = fs
        .read_file_bounded_confined(
            &path_uri,
            turn_environment.cwd(),
            usize::try_from(remaining_upload_bytes).unwrap_or(usize::MAX),
            Some(&sandbox),
        )
        .await
        .map_err(|error| contextualize_error(error.to_string()))?
        .ok_or_else(|| {
            let message = if remaining_upload_bytes < OPENAI_FILE_UPLOAD_LIMIT_BYTES {
                format!(
                    "file is too large, changed while being read, or would make total staged file size exceed the limit of {OPENAI_FILE_UPLOAD_LIMIT_BYTES} bytes"
                )
            } else {
                format!(
                    "file is too large or changed while being read; limit is {OPENAI_FILE_UPLOAD_LIMIT_BYTES} bytes"
                )
            };
            contextualize_error(message)
        })?;
    let file_name = file_path
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("file")
        .to_string();

    Ok(StagedOpenAiFile {
        field_name: field_name.to_string(),
        index,
        file_path: file_path.to_string(),
        file_name,
        contents,
    })
}

fn validate_relative_file_path(
    cwd: &codex_utils_path_uri::PathUri,
    file_path: &str,
) -> Result<(), String> {
    if file_path.is_empty() {
        return Err("file path must not be empty".to_string());
    }
    let convention = cwd
        .infer_path_convention()
        .ok_or_else(|| "selected environment has an unsupported path convention".to_string())?;
    let is_absolute = match convention {
        PathConvention::Posix => file_path.starts_with('/'),
        PathConvention::Windows => {
            file_path.starts_with('/')
                || file_path.starts_with('\\')
                || matches!(file_path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
        }
    };
    if is_absolute {
        return Err(
            "file path must be relative to the selected environment working directory".to_string(),
        );
    }
    let has_parent_component = match convention {
        PathConvention::Posix => file_path.split('/').any(|component| component == ".."),
        PathConvention::Windows => file_path
            .split(['/', '\\'])
            .any(|component| component == ".."),
    };
    if has_parent_component {
        return Err("file path must not contain parent-directory components".to_string());
    }
    Ok(())
}

async fn upload_staged_openai_file(
    base_url: &str,
    auth: &dyn codex_api::AuthProvider,
    http_clients: &codex_http_client::RouteAwareClientPool,
    staged: StagedOpenAiFile,
) -> Result<(JsonValue, String), (String, Option<String>)> {
    let StagedOpenAiFile {
        field_name,
        index,
        file_path,
        file_name,
        contents,
    } = staged;
    let contextualize_error = |error: String| match index {
        Some(index) => {
            format!("failed to upload `{file_path}` for `{field_name}[{index}]`: {error}")
        }
        None => format!("failed to upload `{file_path}` for `{field_name}`: {error}"),
    };
    let file_size_bytes = u64::try_from(contents.len())
        .map_err(|error| (contextualize_error(error.to_string()), None))?;
    let contents = futures::stream::once(async move { Ok::<_, std::io::Error>(contents.into()) });
    let uploaded = upload_openai_file_with_pool(
        base_url,
        auth,
        http_clients,
        file_name,
        file_size_bytes,
        contents,
    )
    .await
    .map_err(|error| {
        let cleanup_file_id = match &error {
            codex_api::OpenAiFileError::RollbackFailed { file_id, .. } => Some(file_id.clone()),
            _ => None,
        };
        (contextualize_error(error.to_string()), cleanup_file_id)
    })?;
    let file_id = uploaded.file_id.clone();
    Ok((
        serde_json::json!({
            "download_url": uploaded.download_url,
            "file_id": uploaded.file_id,
            "mime_type": uploaded.mime_type,
            "file_name": uploaded.file_name,
        }),
        file_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use crate::session::turn_context::TurnEnvironment;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn set_primary_environment_cwd(turn_context: &mut TurnContext, cwd: &Path) {
        let cwd = AbsolutePathBuf::try_from(cwd).expect("absolute path");
        turn_context.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        let primary = turn_context
            .environments
            .turn_environments
            .first_mut()
            .expect("primary environment");
        *primary = TurnEnvironment::new(
            primary.environment_id.clone(),
            Arc::clone(&primary.environment),
            PathUri::from_abs_path(&cwd),
            primary.shell.clone(),
        );
    }

    #[tokio::test]
    async fn rollback_retries_only_unconfirmed_ids_after_draining_independent_deletes() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        for (id, status) in [("one", 204), ("failed", 500), ("three", 204)] {
            Mock::given(method("DELETE"))
                .and(path(format!("/backend-api/files/{id}")))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
        }
        let (session, turn) = make_session_and_context().await;
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        let mut owner = OpenAiUploadCleanup {
            tasks: session.terminal_tasks.clone(),
            base_url: format!("{}/backend-api", server.uri()),
            auth: codex_model_provider::auth_provider_from_auth(&auth),
            clients: openai_file_http_client_pool(&turn.config.http_client_factory()),
            file_ids: vec!["one".into(), "failed".into(), "three".into()],
        };
        owner.rollback().await;
        assert_eq!(owner.file_ids, ["failed"]);
        server.verify().await;
        server.reset().await;
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/failed"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        owner.rollback().await;
        assert!(owner.file_ids.is_empty());
        server.verify().await;
    }

    #[test]
    fn openai_file_paths_must_be_confined_relative_paths() {
        let dir = tempdir().expect("temp dir");
        let cwd = AbsolutePathBuf::try_from(dir.path()).expect("absolute path");
        let cwd = PathUri::from_abs_path(&cwd);

        assert!(validate_relative_file_path(&cwd, "nested/report.csv").is_ok());
        assert!(validate_relative_file_path(&cwd, "").is_err());
        assert!(validate_relative_file_path(&cwd, "../secret.txt").is_err());
        assert!(validate_relative_file_path(&cwd, "nested/../../secret.txt").is_err());
        let absolute = match cwd.infer_path_convention().expect("path convention") {
            PathConvention::Posix => "/etc/passwd",
            PathConvention::Windows => r"C:\\Windows\\win.ini",
        };
        assert!(validate_relative_file_path(&cwd, absolute).is_err());
        if cwd.infer_path_convention() == Some(PathConvention::Windows) {
            for path in [
                r"..\secret.txt",
                r"nested\..\secret.txt",
                r"\secret.txt",
                "C:secret.txt",
            ] {
                assert!(validate_relative_file_path(&cwd, path).is_err(), "{path}");
            }
        }
    }

    #[tokio::test]
    async fn parallel_file_preparation_preserves_order_and_drains_rollback() {
        use std::time::Duration;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::{body_json, method, path};

        for scenario in ["success", "failure", "rollback-failure", "cancelled"] {
            let server = MockServer::start().await;
            let dir = tempdir().unwrap();
            let count = MAX_CONCURRENT_OPENAI_FILE_UPLOADS + 2;
            for index in 0..count {
                let file_name = format!("{index}.csv");
                std::fs::write(dir.path().join(&file_name), b"data").unwrap();
                let allocation = if scenario == "failure" && index == 1 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "file_id": format!("file_{index}"),
                        "upload_url": format!("{}/upload/{index}", server.uri()),
                    }))
                };
                Mock::given(method("POST"))
                    .and(path("/backend-api/files"))
                    .and(body_json(serde_json::json!({"file_name": file_name, "file_size": 4, "use_case": "codex"})))
                    .respond_with(allocation)
                    .mount(&server).await;
                Mock::given(method("PUT"))
                    .and(path(format!("/upload/{index}")))
                    .respond_with(if scenario == "rollback-failure" && index == 1 {
                        ResponseTemplate::new(500)
                    } else {
                        ResponseTemplate::new(200).set_delay(Duration::from_millis(200))
                    })
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(format!("/backend-api/files/file_{index}/uploaded")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "status": "success", "download_url": format!("{}/download/{index}", server.uri()),
                        "file_name": file_name, "mime_type": "text/csv", "file_size_bytes": 4,
                    })))
                    .mount(&server).await;
                let delete_attempts = std::sync::atomic::AtomicUsize::new(0);
                Mock::given(method("DELETE"))
                    .and(path(format!("/backend-api/files/file_{index}")))
                    .respond_with(move |_: &wiremock::Request| {
                        let first =
                            delete_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                        ResponseTemplate::new(
                            if scenario == "rollback-failure" && index == 1 && first {
                                500
                            } else {
                                204
                            },
                        )
                    })
                    .mount(&server)
                    .await;
            }
            let (mut session, mut turn) = make_session_and_context().await;
            session.services.auth_manager = crate::test_support::auth_manager_from_auth(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            );
            set_primary_environment_cwd(&mut turn, dir.path());
            let mut config = (*turn.config).clone();
            config.chatgpt_base_url = format!("{}/backend-api", server.uri());
            turn.config = Arc::new(config);
            let session = Arc::new(session);
            let worker_session = Arc::clone(&session);
            let task = tokio::spawn(async move {
                let step = StepContext::for_test(Arc::new(turn));
                prepare_mcp_tool_arguments_for_openai_files(&worker_session, &step,
                    Some(serde_json::json!({"files": (0..count).map(|i| format!("{i}.csv")).collect::<Vec<_>>(), "unrelated": 42})),
                    Some(&["files".to_string()]),
                ).await
            });
            if scenario == "cancelled" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let requests = server.received_requests().await.unwrap();
                        if requests.iter().filter(|r| r.method == "PUT").count()
                            == MAX_CONCURRENT_OPENAI_FILE_UPLOADS
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("initial upload cohort must overlap before cancellation");
                task.abort();
                assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            } else {
                let result = tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
                if scenario == "success" {
                    let arguments = result.unwrap().dispatched().unwrap();
                    assert_eq!(arguments["unrelated"], 42);
                    for index in 0..count {
                        assert_eq!(
                            arguments["files"][index]["file_id"],
                            format!("file_{index}")
                        );
                    }
                } else {
                    assert!(result.err().unwrap().contains("500"));
                }
            }
            session.terminal_tasks.close();
            tokio::time::timeout(Duration::from_secs(5), session.terminal_tasks.wait())
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let allocations = requests
                .iter()
                .filter(|r| r.method == "POST" && r.url.path() == "/backend-api/files")
                .count();
            if scenario == "success" {
                assert_eq!(allocations, count);
                let second_allocation = requests
                    .iter()
                    .position(|r| {
                        r.method == "POST"
                            && r.body_json::<JsonValue>()
                                .is_ok_and(|body| body["file_name"] == "1.csv")
                    })
                    .unwrap();
                let first_finalize = requests
                    .iter()
                    .position(|r| r.url.path() == "/backend-api/files/file_0/uploaded")
                    .unwrap();
                assert!(
                    second_allocation < first_finalize,
                    "the second file must start before the first file finishes"
                );
                assert!(!requests.iter().any(|r| r.method == "DELETE"));
            } else {
                assert_eq!(
                    allocations, MAX_CONCURRENT_OPENAI_FILE_UPLOADS,
                    "failure/cancellation must not admit another cohort"
                );
                let deleted = requests.iter().filter(|r| r.method == "DELETE").count();
                assert_eq!(
                    deleted,
                    allocations - usize::from(scenario == "failure")
                        + usize::from(scenario == "rollback-failure"),
                    "every late successful upload must be rolled back"
                );
            }
        }
    }

    #[test]
    fn openai_file_staging_budget_rejects_an_aggregate_over_the_upload_limit() {
        let mut budget = OpenAiFileStagingBudget {
            staged_bytes: OPENAI_FILE_UPLOAD_LIMIT_BYTES - 1,
        };

        let error = budget
            .record_file(/*file_size_bytes*/ 2)
            .expect_err("aggregate staging must stay bounded");

        assert!(error.contains("total staged file size exceeds"));
        assert_eq!(
            budget.staged_bytes,
            OPENAI_FILE_UPLOAD_LIMIT_BYTES - 1,
            "a rejected reservation must not consume budget"
        );
        budget.record_file(1).expect("exact limit is allowed");
        assert_eq!(budget.remaining_bytes(), 0);
        budget
            .record_file(0)
            .expect("empty files do not consume budget");
        assert!(budget.record_file(1).is_err());
        assert_eq!(budget.staged_bytes, OPENAI_FILE_UPLOAD_LIMIT_BYTES);
    }

    #[tokio::test]
    async fn openai_file_argument_rewrite_requires_declared_file_params() {
        let (session, turn_context) = make_session_and_context().await;
        let arguments = Some(serde_json::json!({
            "file": "/tmp/codex-smoke-file.txt"
        }));
        let step_context = StepContext::for_test(Arc::new(turn_context));

        let rewritten = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            arguments.clone(),
            /*openai_file_input_params*/ None,
        )
        .await
        .expect("rewrite should succeed");

        assert_eq!(rewritten, arguments);
    }

    #[tokio::test]
    async fn rewrite_rejects_oversized_file() {
        let (mut session, mut turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let dir = tempdir().expect("temp dir");
        let file_path = dir.path().join("oversized.bin");
        let file = std::fs::File::create(&file_path).expect("create sparse file");
        file.set_len(OPENAI_FILE_UPLOAD_LIMIT_BYTES + 1)
            .expect("size sparse file");
        set_primary_environment_cwd(&mut turn_context, dir.path());
        let step_context = StepContext::for_test(Arc::new(turn_context));

        let error = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            Some(serde_json::json!({"file": "oversized.bin"})),
            Some(&["file".to_string()]),
        )
        .await
        .expect_err("oversized file should be rejected");

        assert!(error.contains("is too large"));
        assert!(error.contains(&OPENAI_FILE_UPLOAD_LIMIT_BYTES.to_string()));
    }

    #[tokio::test]
    async fn rewrite_mcp_tool_arguments_for_openai_files_rewrites_scalar_path() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(body_json(serde_json::json!({
                "file_name": "file_report.csv",
                "file_size": 5,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_123",
                "upload_url": format!("{}/upload/file_123", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_123"))
            .and(wiremock::matchers::body_bytes(b"hello".to_vec()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_123/uploaded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success",
                "download_url": format!("{}/download/file_123", server.uri()),
                "file_name": "file_report.csv",
                "mime_type": "text/csv",
                "file_size_bytes": 5,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, mut turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let dir = tempdir().expect("temp dir");
        let local_path = dir.path().join("file_report.csv");
        tokio::fs::write(&local_path, b"hello")
            .await
            .expect("write local file");
        set_primary_environment_cwd(&mut turn_context, dir.path());

        let mut config = (*turn_context.config).clone();
        config.chatgpt_base_url = format!("{}/backend-api", server.uri());
        turn_context.config = Arc::new(config);
        let step_context = StepContext::for_test(Arc::new(turn_context));
        let rewritten = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            Some(serde_json::json!({"file": "file_report.csv", "caption": "unchanged"})),
            Some(&["file".to_string()]),
        )
        .await
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten,
            Some(serde_json::json!({
                "caption": "unchanged",
                "file": {
                    "download_url": format!("{}/download/file_123", server.uri()),
                    "file_id": "file_123",
                    "mime_type": "text/csv",
                    "file_name": "file_report.csv",
                }
            }))
        );
    }

    #[tokio::test]
    async fn cancelled_file_preparation_rolls_back_late_upload() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(body_json(serde_json::json!({
                "file_name": "file_report.csv",
                "file_size": 5,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_123",
                "upload_url": format!("{}/upload/file_123", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_123"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_123/uploaded"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(200))
                    .set_body_json(serde_json::json!({
                        "status": "success",
                        "download_url": format!("{}/download/file_123", server.uri()),
                        "file_name": "file_report.csv",
                        "mime_type": "text/csv",
                        "file_size_bytes": 5,
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, mut turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let dir = tempdir().expect("temp dir");
        let local_path = dir.path().join("file_report.csv");
        tokio::fs::write(&local_path, b"hello")
            .await
            .expect("write local file");
        set_primary_environment_cwd(&mut turn_context, dir.path());

        let mut config = (*turn_context.config).clone();
        config.chatgpt_base_url = format!("{}/backend-api", server.uri());
        turn_context.config = Arc::new(config);
        let step_context = StepContext::for_test(Arc::new(turn_context));
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/file_123"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let session = Arc::new(session);
        let upload_session = Arc::clone(&session);
        let task = tokio::spawn(async move {
            prepare_mcp_tool_arguments_for_openai_files(
                &upload_session,
                &step_context,
                Some(serde_json::json!({"file":"file_report.csv"})),
                Some(&["file".to_string()]),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path().ends_with("/uploaded"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("upload enters delayed acknowledgement");
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        session.terminal_tasks.close();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.terminal_tasks.wait(),
        )
        .await
        .expect("late upload cleanup finishes");
        server.verify().await;
    }

    #[tokio::test]
    async fn rewrite_mcp_tool_arguments_for_openai_files_rewrites_array_paths() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(body_json(serde_json::json!({
                "file_name": "one.csv",
                "file_size": 3,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_1",
                "upload_url": format!("{}/upload/file_1", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(body_json(serde_json::json!({
                "file_name": "two.csv",
                "file_size": 3,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_2",
                "upload_url": format!("{}/upload/file_2", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_2"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_1/uploaded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success",
                "download_url": format!("{}/download/file_1", server.uri()),
                "file_name": "one.csv",
                "mime_type": "text/csv",
                "file_size_bytes": 3,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_2/uploaded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success",
                "download_url": format!("{}/download/file_2", server.uri()),
                "file_name": "two.csv",
                "mime_type": "text/csv",
                "file_size_bytes": 3,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, mut turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let dir = tempdir().expect("temp dir");
        tokio::fs::write(dir.path().join("one.csv"), b"one")
            .await
            .expect("write first local file");
        tokio::fs::write(dir.path().join("two.csv"), b"two")
            .await
            .expect("write second local file");
        set_primary_environment_cwd(&mut turn_context, dir.path());

        let mut config = (*turn_context.config).clone();
        config.chatgpt_base_url = format!("{}/backend-api", server.uri());
        turn_context.config = Arc::new(config);
        let step_context = StepContext::for_test(Arc::new(turn_context));
        let rewritten = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            Some(serde_json::json!({"files": ["one.csv", "two.csv"]})),
            Some(&["files".to_string()]),
        )
        .await
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten,
            Some(serde_json::json!({
                "files": [
                    {
                        "download_url": format!("{}/download/file_1", server.uri()),
                        "file_id": "file_1",
                        "mime_type": "text/csv",
                        "file_name": "one.csv",
                    },
                    {
                        "download_url": format!("{}/download/file_2", server.uri()),
                        "file_id": "file_2",
                        "mime_type": "text/csv",
                        "file_name": "two.csv",
                    }
                ]
            }))
        );
    }

    #[tokio::test]
    async fn rewrite_rolls_back_prior_uploads_when_a_later_upload_fails() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(body_json(serde_json::json!({
                "file_name": "one.csv",
                "file_size": 3,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_1",
                "upload_url": format!("{}/upload/file_1", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_1/uploaded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "success",
                "download_url": format!("{}/download/file_1", server.uri()),
                "file_name": "one.csv",
                "mime_type": "text/csv",
                "file_size_bytes": 3,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(body_json(serde_json::json!({
                "file_name": "two.csv",
                "file_size": 3,
                "use_case": "codex",
            })))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/file_1"))
            .and(header("chatgpt-account-id", "account_id"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let (mut session, mut turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let dir = tempdir().expect("temp dir");
        tokio::fs::write(dir.path().join("one.csv"), b"one")
            .await
            .expect("write first local file");
        tokio::fs::write(dir.path().join("two.csv"), b"two")
            .await
            .expect("write second local file");
        set_primary_environment_cwd(&mut turn_context, dir.path());
        let mut config = (*turn_context.config).clone();
        config.chatgpt_base_url = format!("{}/backend-api", server.uri());
        turn_context.config = Arc::new(config);
        let step_context = StepContext::for_test(Arc::new(turn_context));

        let error = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            Some(serde_json::json!({"files": ["one.csv", "two.csv"]})),
            Some(&["files".to_string()]),
        )
        .await
        .expect_err("second upload should fail");

        assert!(error.contains("500"));
    }

    #[tokio::test]
    async fn rewrite_mcp_tool_arguments_for_openai_files_surfaces_upload_failures() {
        let (mut session, turn_context) = make_session_and_context().await;
        session.services.auth_manager = crate::test_support::auth_manager_from_auth(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let step_context = StepContext::for_test(Arc::new(turn_context));
        let error = rewrite_mcp_tool_arguments_for_openai_files(
            &session,
            &step_context,
            Some(serde_json::json!({
                "file": "definitely/missing/file.csv",
            })),
            Some(&["file".to_string()]),
        )
        .await
        .expect_err("missing file should fail");

        assert!(
            error.contains("failed to upload"),
            "unexpected error: {error}"
        );
        assert!(error.contains("file"));
    }
}
