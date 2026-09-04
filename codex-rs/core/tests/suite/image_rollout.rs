use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageBuffer;
use image::ImageFormat;
use image::Rgb;
use image::Rgba;
use pretty_assertions::assert_eq;
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

const SMALL_PROMPT_IMAGE_DIMENSIONS: (u32, u32) = (1_536, 864);
const LARGE_PNG_DIMENSIONS: (u32, u32) = (2_560, 1_440);
const LARGE_JPEG_DIMENSIONS: (u32, u32) = (3_264, 2_448);

fn find_user_message_with_image(text: &str) -> Option<ResponseItem> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rollout: RolloutLine = match serde_json::from_str(trimmed) {
            Ok(rollout) => rollout,
            Err(_) => continue,
        };
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) =
            &rollout.item
            && role == "user"
            && content
                .iter()
                .any(|span| matches!(span, ContentItem::InputImage { .. }))
            && let RolloutItem::ResponseItem(item) = rollout.item.clone()
        {
            return Some(item);
        }
    }
    None
}

fn extract_image_url(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { content, .. } => content.iter().find_map(|span| match span {
            ContentItem::InputImage { image_url, .. } => Some(image_url.clone()),
            _ => None,
        }),
        _ => None,
    }
}

async fn read_rollout_text(path: &Path) -> anyhow::Result<String> {
    for _ in 0..50 {
        if path.exists()
            && let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return Ok(text);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("read rollout file at {}", path.display()))
}

fn write_test_png(path: &Path, color: [u8; 4]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let image = ImageBuffer::from_pixel(2, 2, Rgba(color));
    image.save(path)?;
    Ok(())
}

fn write_prompt_image_fixture(
    path: &Path,
    dimensions: (u32, u32),
    format: ImageFormat,
    seed: u8,
) -> anyhow::Result<Vec<u8>> {
    let (width, height) = dimensions;
    let image = match format {
        ImageFormat::Png => {
            DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
                Rgba([
                    seed.wrapping_add((x % 251) as u8),
                    seed.wrapping_add((y % 241) as u8),
                    seed.wrapping_add(((x ^ y) % 239) as u8),
                    255,
                ])
            }))
        }
        ImageFormat::Jpeg => {
            DynamicImage::ImageRgb8(ImageBuffer::from_fn(width, height, |x, y| {
                Rgb([
                    seed.wrapping_add((x % 251) as u8),
                    seed.wrapping_add((y % 241) as u8),
                    seed.wrapping_add(((x ^ y) % 239) as u8),
                ])
            }))
        }
        other => anyhow::bail!("unsupported prompt image fixture format: {other:?}"),
    };
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, format)?;
    let bytes = encoded.into_inner();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &bytes)?;
    Ok(bytes)
}

struct OutboundImage {
    mime: String,
    bytes: Vec<u8>,
    dimensions: (u32, u32),
}

fn decode_outbound_image(image_url: &str) -> anyhow::Result<OutboundImage> {
    let (metadata, payload) = image_url
        .split_once(',')
        .context("outbound image should be a data URL")?;
    let mime = metadata
        .strip_prefix("data:")
        .and_then(|metadata| metadata.strip_suffix(";base64"))
        .context("outbound image should use base64 data URL metadata")?;
    let bytes = BASE64_STANDARD.decode(payload)?;
    let dimensions = image::load_from_memory(&bytes)?.dimensions();
    Ok(OutboundImage {
        mime: mime.to_string(),
        bytes,
        dimensions,
    })
}

async fn submit_local_image_turn(test: &TestCodex, path: &Path, text: &str) -> anyhow::Result<()> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd.path());
    test.codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::LocalImage {
                    path: path.to_path_buf(),
                    detail: None,
                },
                UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                },
            ],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(30),
    )
    .await;
    Ok(())
}

async fn shutdown_test_codex(test: &TestCodex) -> anyhow::Result<()> {
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    Ok(())
}

async fn run_fresh_prompt_image_scenario(
    file_name: &str,
    source_dimensions: (u32, u32),
    format: ImageFormat,
    seed: u8,
) -> anyhow::Result<(Vec<u8>, OutboundImage)> {
    // The model endpoint is a process-local loopback fake; no live service is used.
    let server = start_mock_server().await;
    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]);
    let request_log = responses::mount_sse_once(&server, response).await;
    let test = test_codex().build(&server).await?;
    let image_path = test.cwd.path().join("prompt-images").join(file_name);
    let source_bytes = write_prompt_image_fixture(&image_path, source_dimensions, format, seed)?;

    submit_local_image_turn(&test, &image_path, "inspect this image").await?;
    shutdown_test_codex(&test).await?;

    let request = request_log.single_request();
    let image_urls = request.message_input_image_urls("user");
    assert_eq!(image_urls.len(), 1, "expected one outbound user image");
    Ok((source_bytes, decode_outbound_image(&image_urls[0])?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_images_small_png_screenshot_fresh_attachment() -> anyhow::Result<()> {
    let (source_bytes, outbound) = run_fresh_prompt_image_scenario(
        "small-fresh.png",
        SMALL_PROMPT_IMAGE_DIMENSIONS,
        ImageFormat::Png,
        11,
    )
    .await?;

    assert_eq!(outbound.mime, "image/png");
    assert_eq!(outbound.dimensions, SMALL_PROMPT_IMAGE_DIMENSIONS);
    assert_eq!(outbound.bytes, source_bytes);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_images_large_png_screenshot_fresh_attachment() -> anyhow::Result<()> {
    let (source_bytes, outbound) = run_fresh_prompt_image_scenario(
        "large-fresh.png",
        LARGE_PNG_DIMENSIONS,
        ImageFormat::Png,
        29,
    )
    .await?;

    assert_eq!(outbound.mime, "image/png");
    assert_eq!(outbound.dimensions, (2_048, 1_152));
    assert_ne!(outbound.bytes, source_bytes);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_images_large_jpeg_photo_fresh_attachment() -> anyhow::Result<()> {
    let (source_bytes, outbound) = run_fresh_prompt_image_scenario(
        "large-fresh.jpg",
        LARGE_JPEG_DIMENSIONS,
        ImageFormat::Jpeg,
        47,
    )
    .await?;

    assert_eq!(outbound.mime, "image/jpeg");
    assert_eq!(outbound.dimensions, (1_824, 1_368));
    assert_ne!(outbound.bytes, source_bytes);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_images_small_png_screenshot_repeated_attachment() -> anyhow::Result<()> {
    // Both turns hit the real session-to-model request path against local loopback.
    let server = start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "first done"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "second done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build(&server).await?;
    let image_path = test
        .cwd
        .path()
        .join("prompt-images")
        .join("small-repeated.png");
    let source_bytes = write_prompt_image_fixture(
        &image_path,
        SMALL_PROMPT_IMAGE_DIMENSIONS,
        ImageFormat::Png,
        83,
    )?;

    submit_local_image_turn(&test, &image_path, "first attachment").await?;
    submit_local_image_turn(&test, &image_path, "second attachment").await?;
    shutdown_test_codex(&test).await?;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 2, "expected one model request per turn");
    let first_urls = requests[0].message_input_image_urls("user");
    let second_urls = requests[1].message_input_image_urls("user");
    assert_eq!(first_urls.len(), 1);
    assert_eq!(second_urls.len(), 2);
    for image_url in first_urls.iter().chain(&second_urls) {
        let outbound = decode_outbound_image(image_url)?;
        assert_eq!(outbound.mime, "image/png");
        assert_eq!(outbound.dimensions, SMALL_PROMPT_IMAGE_DIMENSIONS);
        assert_eq!(outbound.bytes, source_bytes);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_paste_local_image_persists_rollout_request_shape() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex {
        codex,
        cwd,
        session_configured,
        home: _home,
        ..
    } = test_codex().build(&server).await?;

    let rel_path = "images/paste.png";
    let abs_path = cwd.path().join(rel_path);
    write_test_png(&abs_path, [12, 34, 56, 255])?;

    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]);
    responses::mount_sse_once(&server, response).await;

    let session_model = session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::LocalImage {
                    path: abs_path.clone(),
                    detail: None,
                },
                UserInput::Text {
                    text: "pasted image".to_string(),
                    text_elements: Vec::new(),
                },
            ],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let rollout_path = codex.rollout_path().expect("rollout path");
    let rollout_text = read_rollout_text(&rollout_path).await?;
    let actual = find_user_message_with_image(&rollout_text)
        .expect("expected user message with input image in rollout");

    let image_url = extract_image_url(&actual).expect("expected image url in rollout");
    let expected = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: codex_protocol::models::local_image_open_tag_text_with_path(
                    /*label_number*/ 1, &abs_path,
                ),
            },
            ContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            ContentItem::InputText {
                text: codex_protocol::models::image_close_tag_text(),
            },
            ContentItem::InputText {
                text: "pasted image".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        responses::strip_response_item_ids(&[responses::strip_metadata(actual)]),
        responses::strip_response_item_ids(&[expected])
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drag_drop_image_persists_rollout_request_shape() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex {
        codex,
        cwd,
        session_configured,
        home: _home,
        ..
    } = test_codex().build(&server).await?;

    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string();

    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]);
    responses::mount_sse_once(&server, response).await;

    let session_model = session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Image {
                    image_url: image_url.clone(),
                    detail: None,
                },
                UserInput::Text {
                    text: "dropped image".to_string(),
                    text_elements: Vec::new(),
                },
            ],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let rollout_path = codex.rollout_path().expect("rollout path");
    let rollout_text = read_rollout_text(&rollout_path).await?;
    let actual = find_user_message_with_image(&rollout_text)
        .expect("expected user message with input image in rollout");

    let image_url = extract_image_url(&actual).expect("expected image url in rollout");
    let expected = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            ContentItem::InputText {
                text: "dropped image".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        responses::strip_response_item_ids(&[responses::strip_metadata(actual)]),
        responses::strip_response_item_ids(&[expected])
    );

    Ok(())
}
