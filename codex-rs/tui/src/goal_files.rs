//! Materializes oversized TUI goal objectives, pastes, and images as app-server-host files.

use std::path::Path;

use crate::app_server_session::AppServerSession;
use crate::bottom_pane::ChatComposer;
use crate::bottom_pane::LocalImageAttachment;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_app_server_client::AppServerPath;
use codex_protocol::protocol::MAX_THREAD_GOAL_OBJECTIVE_CHARS;
use codex_protocol::user_input::TextElement;
use uuid::Uuid;

const GOAL_ATTACHMENT_DIR: &str = "attachments";
const GOAL_FILE_PREFIX: &str = "Read the Codex goal objective file at ";
const GOAL_FILE_SUFFIX: &str = " before continuing.";
const GOAL_FILE_NAME: &str = "goal-objective.md";

#[derive(Clone, Debug, Default)]
pub(crate) struct GoalDraft {
    pub(crate) objective: String,
    pub(crate) text_elements: Vec<TextElement>,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub(crate) local_images: Vec<LocalImageAttachment>,
    pub(crate) remote_image_urls: Vec<String>,
}

pub(crate) type GoalFilePath = AppServerPath;

pub(crate) async fn materialize_goal_draft(
    app_server: &mut AppServerSession,
    codex_home: Option<&GoalFilePath>,
    draft: GoalDraft,
) -> Result<(String, Option<GoalFilePath>)> {
    let mut objective = draft.objective;
    if objective.trim().is_empty() {
        bail!("Goal objective must not be empty.");
    }
    let text_elements = draft.text_elements;
    if !draft.pending_pastes.is_empty() {
        // Preserve the expansion's placeholder ordering and matching semantics,
        // but replace each payload with only its whitespace/non-whitespace signal.
        let validation_pastes = draft
            .pending_pastes
            .iter()
            .map(|(placeholder, text)| {
                (
                    placeholder.clone(),
                    if text.trim().is_empty() {
                        String::new()
                    } else {
                        "x".to_string()
                    },
                )
            })
            .collect::<Vec<_>>();
        let (expanded_objective, _) = ChatComposer::expand_pending_pastes(
            &objective,
            text_elements.clone(),
            &validation_pastes,
        );
        if expanded_objective.trim().is_empty() {
            bail!("Goal objective must not be empty.");
        }
    }

    let mut active_placeholders = text_elements
        .iter()
        .filter_map(|element| element.placeholder(&objective))
        .filter(|placeholder| !placeholder.is_empty())
        .collect::<Vec<_>>();
    let mut output_dir = None;
    let mut paste_files = Vec::new();
    let mut image_files = Vec::new();
    let mut replacements = Vec::new();
    for (placeholder, text) in draft.pending_pastes.iter() {
        let Some(active_idx) = active_placeholders
            .iter()
            .position(|active| *active == placeholder.as_str())
        else {
            continue;
        };
        active_placeholders.swap_remove(active_idx);
        let path = goal_output_dir(codex_home, &mut output_dir)?
            .join(format!("pasted-text-{}.txt", replacements.len() + 1));
        paste_files.push((path.clone(), text.as_bytes()));

        replacements.push((
            placeholder.clone(),
            format!("pasted text file: {path}. Read this file before continuing."),
        ));
    }

    let mut image_lines = Vec::new();
    for (idx, image) in draft.local_images.iter().enumerate() {
        if !image.placeholder.is_empty() {
            let Some(active_idx) = active_placeholders
                .iter()
                .position(|active| *active == image.placeholder.as_str())
            else {
                continue;
            };
            active_placeholders.swap_remove(active_idx);
        }
        let extension = image_extension(&image.path);
        let path = goal_output_dir(codex_home, &mut output_dir)?.join(format!(
            "image-{}.{}",
            idx + 1,
            extension
        ));
        image_files.push((path.clone(), &image.path));
        if image.placeholder.is_empty() {
            image_lines.push(format!("- [Image #{}]: {path}", idx + 1));
        } else {
            replacements.push((image.placeholder.clone(), format!("image file: {path}")));
        }
    }

    let (expanded_objective, _) =
        ChatComposer::expand_pending_pastes(&objective, text_elements, &replacements);
    objective = expanded_objective.trim().to_string();
    append_section(&mut objective, "Referenced image files:", image_lines);

    append_section(
        &mut objective,
        "Referenced image URLs:",
        draft
            .remote_image_urls
            .into_iter()
            .enumerate()
            .map(|(idx, url)| format!("- [Image #{}]: {url}", idx + 1))
            .collect(),
    );

    let objective_file = if objective.chars().count() > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        let path = goal_output_dir(codex_home, &mut output_dir)?.join(GOAL_FILE_NAME);
        let reference = objective_file_reference(&path)?;
        Some((path, std::mem::replace(&mut objective, reference)))
    } else {
        None
    };

    // Validate the final reference before creating any directory or writing attachments.
    // Borrow paste payloads and image source paths until this point; do not buffer image bytes.
    let materialized: Result<()> = async {
        if let Some(path) = output_dir.as_ref() {
            app_server
                .fs_create_directory_all_path(path)
                .await
                .map_err(|err| anyhow::anyhow!("{err}"))
                .with_context(|| format!("Could not create goal attachment directory {path}"))?;
        }
        for (path, bytes) in paste_files {
            write_goal_file(app_server, path, bytes.to_vec()).await?;
        }
        for (path, source) in image_files {
            let bytes = tokio::fs::read(source)
                .await
                .with_context(|| format!("Could not read goal image {}", source.display()))?;
            write_goal_file(app_server, path, bytes).await?;
        }
        if let Some((path, content)) = objective_file {
            write_goal_file(app_server, path, content.into_bytes()).await?;
        }
        Ok(())
    }
    .await;
    if let Err(err) = materialized {
        // No goal references this fresh directory yet. Roll back partial writes without
        // buffering all images, preserving the original error even if cleanup fails.
        if let Some(path) = output_dir.as_ref() {
            cleanup_uncommitted_goal_files(app_server, path).await;
        }
        return Err(err);
    }
    Ok((objective, output_dir))
}

/// Best-effort rollback of a fresh attachment directory known not to belong to a saved goal.
pub(crate) async fn cleanup_uncommitted_goal_files(
    app_server: &mut AppServerSession,
    path: &GoalFilePath,
) {
    // A remote server may stop responding after rejecting a write or goal save. Cleanup
    // must not indefinitely delay the original error. A dispatched remove may finish later.
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app_server.fs_remove_path(path),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!("failed to clean up uncommitted goal files at {path}: {err}");
        }
        Err(_) => {
            tracing::warn!("timed out cleaning up uncommitted goal files at {path}");
        }
    }
}

pub(crate) async fn objective_text_for_edit(
    app_server: &mut AppServerSession,
    codex_home: Option<&GoalFilePath>,
    objective: &str,
) -> Result<String> {
    let Some(path) = objective_file_path(objective, codex_home) else {
        return Ok(objective.to_string());
    };
    let bytes = app_server
        .fs_read_file_path(&path)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
        .with_context(|| format!("Could not read goal objective file {path}"))?;
    String::from_utf8(bytes)
        .with_context(|| format!("Goal objective file {path} is not valid UTF-8"))
}

pub(crate) fn objective_file_path(
    objective: &str,
    codex_home: Option<&GoalFilePath>,
) -> Option<GoalFilePath> {
    let path = objective
        .strip_prefix(GOAL_FILE_PREFIX)
        .and_then(|path| path.strip_suffix(GOAL_FILE_SUFFIX))?;
    let path = AppServerPath::from_absolute_str(path)?;
    let parts = path.components();
    let attachment_id = parts.get(parts.len().checked_sub(2)?)?;
    let expected = codex_home?
        .join(GOAL_ATTACHMENT_DIR)
        .join(attachment_id)
        .join(GOAL_FILE_NAME);
    (path == expected && Uuid::parse_str(attachment_id).is_ok()).then_some(path)
}

pub(crate) fn objective_file_reference(path: &GoalFilePath) -> Result<String> {
    let reference = format!("{GOAL_FILE_PREFIX}{path}{GOAL_FILE_SUFFIX}");
    let actual_chars = reference.chars().count();
    if actual_chars > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        bail!(
            "Goal objective file reference is too long: {actual_chars} characters. Limit: {MAX_THREAD_GOAL_OBJECTIVE_CHARS} characters."
        );
    }
    Ok(reference)
}

fn goal_output_dir(
    codex_home: Option<&GoalFilePath>,
    output_dir: &mut Option<GoalFilePath>,
) -> Result<GoalFilePath> {
    if let Some(output_dir) = output_dir {
        return Ok(output_dir.clone());
    }
    let codex_home = codex_home
        .context("App server did not report $CODEX_HOME; cannot materialize goal files")?;
    let path = codex_home
        .join(GOAL_ATTACHMENT_DIR)
        .join(Uuid::new_v4().to_string());
    *output_dir = Some(path.clone());
    Ok(path)
}

async fn write_goal_file(
    app_server: &mut AppServerSession,
    path: GoalFilePath,
    bytes: Vec<u8>,
) -> Result<()> {
    app_server
        .fs_write_file_path(&path, bytes)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
        .with_context(|| format!("Could not write goal file {path}"))
}
fn append_section(objective: &mut String, heading: &str, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    if !objective.ends_with('\n') {
        objective.push_str("\n\n");
    }
    objective.push_str(heading);
    objective.push('\n');
    objective.push_str(&lines.join("\n"));
}

fn image_extension(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extension
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(8)
                .collect::<String>()
        })
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "png".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_client::AppServerClient;
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn materialization_preserves_write_error_when_cleanup_fails_or_stalls() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("ws://{}", listener.local_addr()?);
        let (release, released) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket");
            let initialize = socket.next().await.expect("initialize").expect("frame");
            let initialize: serde_json::Value =
                serde_json::from_str(initialize.to_text().expect("text")).expect("JSON");
            assert_eq!(initialize["method"], "initialize");
            socket
                .send(Message::Text(
                    serde_json::json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("initialize response");
            let initialized = socket.next().await.expect("initialized").expect("frame");
            let initialized: serde_json::Value =
                serde_json::from_str(initialized.to_text().expect("text")).expect("JSON");
            assert_eq!(initialized["method"], "initialized");
            for cleanup_stalls in [false, true] {
                let mut created_path = serde_json::Value::Null;
                for method in ["fs/createDirectory", "fs/writeFile", "fs/remove"] {
                    let request = tokio::time::timeout(Duration::from_secs(5), socket.next())
                        .await
                        .expect("request must arrive")
                        .expect("request")
                        .expect("frame");
                    let request: serde_json::Value =
                        serde_json::from_str(request.to_text().expect("text")).expect("JSON");
                    assert_eq!(request["method"], method);
                    if method == "fs/createDirectory" {
                        created_path = request["params"]["path"].clone();
                        let attachment_id = created_path
                            .as_str()
                            .and_then(|path| path.strip_prefix("/remote/codex/attachments/"))
                            .expect("fresh directory must be a child of the attachment root");
                        assert!(Uuid::parse_str(attachment_id).is_ok(), "{attachment_id}");
                    } else if method == "fs/remove" {
                        // Roll back exactly this draft's fresh directory, never the home
                        // or attachment root which may contain previously committed goals.
                        assert_eq!(request["params"]["path"], created_path);
                        if cleanup_stalls {
                            continue;
                        }
                    }
                    let response = if method == "fs/createDirectory" {
                        serde_json::json!({"id": request["id"], "result": {}})
                    } else {
                        serde_json::json!({"id": request["id"], "error": {
                            "code": -32000,
                            "message": if method == "fs/writeFile" { "write denied" } else { "cleanup denied" }
                        }})
                    };
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .expect("response");
                }
            }
            tokio::time::timeout(Duration::from_secs(5), released)
                .await
                .expect("materialization must return despite unanswered remove")
                .expect("release peer");
        });
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: endpoint,
                auth_token: None,
            },
            client_name: "codex-tui-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 8,
        })
        .await?;
        let mut session =
            AppServerSession::new(AppServerClient::Remote(client), ThreadParamsMode::Remote);
        let home = AppServerPath::from_app_server("/remote/codex");
        for _ in 0..2 {
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                materialize_goal_draft(
                    &mut session,
                    Some(&home),
                    GoalDraft {
                        objective: "x".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS + 1),
                        ..Default::default()
                    },
                ),
            )
            .await
            .expect("cleanup must not block the original failure")
            .expect_err("write failure must not become success");
            let error = format!("{error:#}");
            assert!(error.contains("Could not write goal file"), "{error}");
            assert!(error.contains("write denied"), "{error}");
            assert!(!error.contains("cleanup denied"), "{error}");
        }
        release.send(()).expect("release peer");
        peer.await?;
        session
            .shutdown()
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }
}
