use codex_api::ImageBackground;
use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageQuality;
use codex_api::ImageUrl;
use codex_core::context::extension_image_generation_output_hint;
use codex_extension_api::ConversationHistoryRequirement;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ResponsesApiNamespaceTool;
use pretty_assertions::assert_eq;

use super::GeneratedImageOutput;
use super::ImageRequest;
use super::ImagegenArgs;
use super::image_history_requirement;
use super::imagegen_tool_spec;
use super::request_for_call_args;
use crate::IMAGE_GEN_NAMESPACE;
use crate::IMAGEGEN_TOOL_NAME;

const RESULT: &str = "cG5n";

const VALID_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[derive(Default)]
struct RecordingImageEmitter(std::sync::Mutex<Vec<codex_extension_api::ExtensionTurnItem>>);

impl codex_extension_api::TurnItemEmitter for RecordingImageEmitter {
    fn emit_started<'a>(
        &'a self,
        _item: codex_extension_api::ExtensionTurnItem,
    ) -> codex_extension_api::TurnItemEmissionFuture<'a> {
        Box::pin(async {})
    }

    fn emit_completed<'a>(
        &'a self,
        item: codex_extension_api::ExtensionTurnItem,
    ) -> codex_extension_api::TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            self.0.lock().unwrap().push(item);
        })
    }
}

#[tokio::test]
async fn image_completion_requires_valid_bytes_regardless_of_save_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(temp.path()).unwrap();
    let blocked = root.join("blocked");
    std::fs::write(blocked.as_path(), "not a directory").unwrap();
    for save_root in [None, Some(&root), Some(&blocked)] {
        for result in ["", "%%%", RESULT, VALID_PNG] {
            let emitter = RecordingImageEmitter::default();
            let output = super::complete_image_generation(
                "call",
                &emitter,
                "prompt".to_string(),
                result.to_string(),
                save_root,
                "thread",
            )
            .await;
            let valid = result == VALID_PNG;
            assert_eq!(output.is_ok(), valid);
            let items = emitter.0.lock().unwrap();
            assert_eq!(items.len(), 1);
            let codex_extension_items::ExtensionItem::ImageGeneration(item) = &items[0].item else {
                panic!("image completion")
            };
            assert_eq!(item.status, if valid { "completed" } else { "failed" });
            assert_eq!(item.result.is_empty(), !valid);
            if let Ok(output) = output {
                assert_eq!(
                    output.code_mode_result(&function_payload())["image_url"],
                    format!("data:image/png;base64,{VALID_PNG}")
                );
                if save_root == Some(&blocked) {
                    assert!(
                        output.code_mode_result(&function_payload())["output_hint"]
                            .as_str()
                            .unwrap()
                            .contains("saving it on the Codex host failed")
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn referenced_image_reads_are_bounded_without_changing_valid_bytes() {
    use base64::Engine;
    let temp = tempfile::tempdir().unwrap();
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::try_from(temp.path()).unwrap();
    let path = cwd.join("reference.png");
    let environment = codex_extension_api::ToolEnvironment {
        environment_id: "local".to_string(),
        cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        file_system: codex_exec_server::LOCAL_FS.clone(),
        file_system_sandbox_context:
            codex_exec_server::FileSystemSandboxContext::from_legacy_sandbox_policy(
                codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
                codex_utils_path_uri::PathUri::from_abs_path(&cwd),
            )
            .unwrap(),
    };
    std::fs::write(
        path.as_path(),
        super::BASE64_STANDARD.decode(VALID_PNG).unwrap(),
    )
    .unwrap();
    assert_eq!(
        super::image_url(&path, &environment)
            .await
            .unwrap()
            .image_url,
        format!("data:image/png;base64,{VALID_PNG}")
    );
    std::fs::File::options()
        .write(true)
        .open(path.as_path())
        .unwrap()
        .set_len(super::MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 + 1)
        .unwrap();
    let error = super::image_url(&path, &environment)
        .await
        .expect_err("source exceeds read limit");
    assert!(error.to_string().contains("byte limit"));
}

#[test]
fn generated_image_validation_rejects_unusable_provider_results() {
    use base64::Engine;
    for invalid in [
        String::new(),
        "%%%".to_string(),
        RESULT.to_string(),
        super::BASE64_STANDARD.encode(b"\x89PNG\r\n\x1a\ntruncated"),
    ] {
        assert!(super::validate_generated_image(invalid).is_err());
    }
    let (result, bytes) =
        super::validate_generated_image(format!(" {VALID_PNG}\n")).expect("valid provider PNG");
    assert_eq!(result, VALID_PNG);
    assert_eq!(bytes, super::BASE64_STANDARD.decode(VALID_PNG).unwrap());
}

#[tokio::test]
async fn generated_image_persistence_reports_each_delivery_outcome() {
    let (_, bytes) = super::validate_generated_image(VALID_PNG.to_string()).unwrap();
    let fs = codex_exec_server::LOCAL_FS.as_ref();
    let (path, hint) =
        super::persist_generated_image(fs, None, "thread", "call", bytes.clone()).await;
    assert_eq!(path, None);
    assert!(hint.contains("not configured"));
    let temp = tempfile::tempdir().unwrap();
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(temp.path()).unwrap();
    let (path, hint) =
        super::persist_generated_image(fs, Some(&root), "thread", "call", bytes.clone()).await;
    let path = path.expect("saved artifact");
    assert_eq!(std::fs::read(path.as_path()).unwrap(), bytes);
    assert!(hint.contains("host filesystem"));
    let blocked = root.join("blocked");
    std::fs::write(blocked.as_path(), "not a directory").unwrap();
    let (path, hint) =
        super::persist_generated_image(fs, Some(&blocked), "thread", "call", bytes).await;
    assert_eq!(path, None);
    assert!(hint.contains("saving it on the Codex host failed"));
    assert!(hint.contains("returned image remains available"));
}

#[test]
fn requests_history_only_for_history_backed_edits() {
    let payload = |num_last_images_to_include| ToolPayload::Function {
        arguments: serde_json::json!({
            "prompt": "paint a moonlit lake",
            "num_last_images_to_include": num_last_images_to_include,
        })
        .to_string(),
    };

    assert_eq!(
        image_history_requirement(&payload(None::<usize>)),
        ConversationHistoryRequirement::None
    );
    assert_eq!(
        image_history_requirement(&payload(Some(2))),
        ConversationHistoryRequirement::Full
    );
    for count in [0, 6, usize::MAX] {
        assert_eq!(
            image_history_requirement(&payload(Some(count))),
            ConversationHistoryRequirement::None
        );
    }
    for arguments in [
        "{".to_string(),
        serde_json::json!({"num_last_images_to_include": 1}).to_string(),
        serde_json::json!({"prompt": "edit", "unknown": true, "num_last_images_to_include": 1}).to_string(),
        serde_json::json!({"prompt": "edit", "referenced_image_paths": ["/tmp/image.png"], "num_last_images_to_include": 1}).to_string(),
    ] {
        assert_eq!(image_history_requirement(&ToolPayload::Function { arguments }), ConversationHistoryRequirement::None);
    }
}

#[test]
fn uses_reserved_image_gen_namespace() {
    let ToolSpec::Namespace(spec) = imagegen_tool_spec() else {
        panic!("imagegen should advertise a namespace tool");
    };
    assert_eq!(spec.name, IMAGE_GEN_NAMESPACE);
    let ResponsesApiNamespaceTool::Function(function) = &spec.tools[0];
    assert_eq!(function.name, IMAGEGEN_TOOL_NAME);
}

#[tokio::test]
async fn omitted_references_generate_with_fixed_defaults() {
    assert_eq!(
        request_for_call_args(
            &ImagegenArgs {
                prompt: "paint a moonlit lake".to_string(),
                referenced_image_paths: None,
                num_last_images_to_include: None,
            },
            &[],
            None,
            &[],
        )
        .await
        .expect("generation request should build"),
        ImageRequest::Generate(ImageGenerationRequest {
            prompt: "paint a moonlit lake".to_string(),
            background: Some(ImageBackground::Auto),
            model: "gpt-image-2".to_string(),
            n: None,
            quality: Some(ImageQuality::Auto),
            size: Some("auto".to_string()),
        })
    );
}

#[tokio::test]
async fn recent_image_fallback_selects_newest_images_in_chronological_order() {
    let history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                input_image("user-1"),
                input_image("user-2"),
                ContentItem::InputText {
                    text: "edit these".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "mcp_image".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "mcp-call".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "mcp-call".to_string(),
            output: image_output("mcp"),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".to_string()),
            call_id: "code-mode-call".to_string(),
            name: "exec".to_string(),
            namespace: None,
            input: String::new(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "code-mode-call".to_string(),
            name: Some("exec".to_string()),
            output: image_output("code-mode"),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ImageGenerationCall {
            id: Some(ResponseItemId::with_suffix("ig", "generated-call")),
            status: "completed".to_string(),
            revised_prompt: None,
            result: "generated".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "orphan-call".to_string(),
            output: image_output("orphan"),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    assert_eq!(
        request_for_call_args(
            &ImagegenArgs {
                prompt: "change the lighting".to_string(),
                referenced_image_paths: None,
                num_last_images_to_include: Some(4),
            },
            &history,
            None,
            &[],
        )
        .await
        .expect("history-backed edit request should build"),
        ImageRequest::Edit(expected_edit_request(
            "change the lighting",
            &["user-2", "mcp", "code-mode", "generated"],
        ))
    );
}

#[tokio::test]
async fn conflicting_image_selectors_return_tool_error() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: Some(vec![
                "/tmp/image.png"
                    .try_into()
                    .expect("test path should be absolute"),
            ]),
            num_last_images_to_include: Some(1),
        },
        &[],
        None,
        &[],
    )
    .await
    .expect_err("conflicting selectors should fail");

    assert_eq!(
        error.to_string(),
        "provide only one of `referenced_image_paths` or `num_last_images_to_include`"
    );
}

#[tokio::test]
async fn too_many_referenced_image_paths_return_tool_error() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: Some(
                (0..6)
                    .map(|index| {
                        format!("/tmp/image-{index}.png")
                            .try_into()
                            .expect("test path should be absolute")
                    })
                    .collect(),
            ),
            num_last_images_to_include: None,
        },
        &[],
        None,
        &[],
    )
    .await
    .expect_err("too many paths should fail before reading files");

    assert_eq!(
        error.to_string(),
        "`referenced_image_paths` must contain at most 5 paths"
    );
}

#[tokio::test]
async fn recent_image_fallback_requires_requested_count() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: None,
            num_last_images_to_include: Some(2),
        },
        &[ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![input_image("only-image")],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        None,
        &[],
    )
    .await
    .expect_err("history-backed edit should require the requested image count");

    assert_eq!(
        error.to_string(),
        "requested the last 2 conversation images, but only 1 were available"
    );
}

#[tokio::test]
async fn referenced_paths_reject_an_unreadable_primary_environment() {
    let cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(std::env::current_dir().unwrap())
            .unwrap();
    let sandbox = codex_exec_server::FileSystemSandboxContext::from_legacy_sandbox_policy(
        codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
        codex_utils_path_uri::PathUri::from_abs_path(&cwd),
    )
    .unwrap();
    let environments = [codex_extension_api::ToolEnvironment {
        environment_id: "readable-alternative".to_string(),
        cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        file_system: codex_exec_server::LOCAL_FS.clone(),
        file_system_sandbox_context: sandbox,
    }];
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: Some(vec![
                "/tmp/image.png"
                    .try_into()
                    .expect("test path should be absolute"),
            ]),
            num_last_images_to_include: None,
        },
        &[],
        Some("foreign-primary"),
        &environments,
    )
    .await
    .expect_err("an omitted primary environment must not fall back to another environment");

    assert_eq!(
        error.to_string(),
        "referenced image paths are unavailable because the primary environment is not extension-readable"
    );
}

#[test]
fn generated_output_returns_image_input_and_output_hint() {
    let output_hint =
        extension_image_generation_output_hint("/tmp", "/tmp/call-1.png").expect("hint should fit");
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: Some(output_hint.clone()),
    };

    let ResponseInputItem::FunctionCallOutput {
        output: response_output,
        ..
    } = output.to_response_item("call-1", &function_payload())
    else {
        panic!("imagegen should return function tool output");
    };
    let FunctionCallOutputBody::ContentItems(content_items) = response_output.body else {
        panic!("imagegen output should contain generated image bytes");
    };
    assert_eq!(
        content_items,
        vec![
            FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:image/png;base64,{RESULT}"),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText { text: output_hint },
        ]
    );
}

#[test]
fn generated_output_returns_generated_image_helper_input_in_code_mode() {
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: Some("generated image save hint".to_string()),
    };

    assert_eq!(
        output.code_mode_result(&function_payload()),
        serde_json::json!({
            "image_url": format!("data:image/png;base64,{RESULT}"),
            "output_hint": "generated image save hint",
        })
    );
}

#[test]
fn generated_output_preserves_copy_instruction_for_oversized_paths() {
    let long_path = "x".repeat(1024);
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: extension_image_generation_output_hint("/tmp", long_path),
    };

    let expected_hint = "If you need to use a generated image at another path, copy it and leave the original in place unless the user explicitly asks you to delete it.";
    assert_eq!(
        output.code_mode_result(&function_payload()),
        serde_json::json!({
            "image_url": format!("data:image/png;base64,{RESULT}"),
            "output_hint": expected_hint,
        })
    );

    let ResponseInputItem::FunctionCallOutput {
        output: response_output,
        ..
    } = output.to_response_item("call-1", &function_payload())
    else {
        panic!("imagegen should return function tool output");
    };
    let FunctionCallOutputBody::ContentItems(content_items) = response_output.body else {
        panic!("imagegen output should contain generated image bytes");
    };
    assert_eq!(
        content_items,
        vec![
            FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:image/png;base64,{RESULT}"),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: expected_hint.to_string(),
            },
        ]
    );
}

fn input_image(image: &str) -> ContentItem {
    ContentItem::InputImage {
        image_url: format!("data:image/png;base64,{image}"),
        detail: None,
    }
}

fn image_output(image: &str) -> FunctionCallOutputPayload {
    FunctionCallOutputPayload::from_content_items(vec![FunctionCallOutputContentItem::InputImage {
        image_url: format!("data:image/png;base64,{image}"),
        detail: None,
    }])
}

fn expected_edit_request(prompt: &str, images: &[&str]) -> ImageEditRequest {
    ImageEditRequest {
        images: images
            .iter()
            .map(|image| ImageUrl {
                image_url: format!("data:image/png;base64,{image}"),
            })
            .collect(),
        prompt: prompt.to_string(),
        background: Some(ImageBackground::Auto),
        model: "gpt-image-2".to_string(),
        n: None,
        quality: Some(ImageQuality::Auto),
        size: Some("auto".to_string()),
    }
}

fn function_payload() -> ToolPayload {
    ToolPayload::Function {
        arguments: "{}".to_string(),
    }
}
