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
use codex_api::OPENAI_FILE_UPLOAD_LIMIT_BYTES;
use codex_api::delete_openai_file_with_pool;
use codex_api::openai_file_http_client_pool;
use codex_api::upload_openai_file_bytes_with_pool;
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
    let uploaded = upload_openai_file_bytes_with_pool(
        base_url,
        auth,
        http_clients,
        file_name,
        contents.into(),
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
