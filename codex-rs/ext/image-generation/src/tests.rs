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

const RESULT: &str = "cG5n";

const VALID_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[tokio::test]
async fn compacted_recent_image_request_never_substitutes_an_older_survivor() {
    let image = |url: &str| ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputImage {
            image_url: url.into(),
            detail: None,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let args = ImagegenArgs {
        prompt: "edit latest".into(),
        referenced_image_paths: None,
        num_last_images_to_include: Some(1),
    };
    for checkpoint in [
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "checkpoint".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: format!("{}\nsummary", codex_core::compact::SUMMARY_PREFIX),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ] {
        // A survives, but newer B was evicted. Never silently edit A.
        let mut history = vec![image("older-A"), checkpoint];
        assert!(
            request_for_call_args(&args, &history, None, &[])
                .await
                .is_err()
        );
        history.push(image("new-C"));
        let ImageRequest::Edit(request) = request_for_call_args(&args, &history, None, &[])
            .await
            .unwrap()
        else {
            panic!("edit request")
        };
        assert_eq!(
            request.images,
            vec![ImageUrl {
                image_url: "new-C".into()
            }]
        );
        let args = ImagegenArgs {
            num_last_images_to_include: Some(2),
            prompt: args.prompt.clone(),
            referenced_image_paths: None,
        };
        assert!(
            request_for_call_args(&args, &history, None, &[])
                .await
                .is_err()
        );
    }
}

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
            let usage = codex_extension_items::image_generation::ImageGenerationUsage {
                model: Some("image-fixture".into()),
                reported: true,
                input_tokens: Some(1474),
                output_tokens: Some(1372),
                total_tokens: Some(2846),
                ..Default::default()
            };
            let output = super::complete_image_generation(
                "call",
                &emitter,
                "prompt".to_string(),
                result.to_string(),
                save_root,
                "thread",
                Some(usage.clone()),
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
            assert_eq!(
                item.provider_usage,
                Some(usage),
                "receipt survives decode and save failures"
            );
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
        super::image_url(&path.display().to_string(), &environment)
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
    let error = super::image_url(&path.display().to_string(), &environment)
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
    let arguments = serde_json::json!({
        "prompt": "edit",
        "referenced_image_paths": [std::env::current_dir().unwrap().join("image.png")],
        "num_last_images_to_include": 1,
    })
    .to_string();
    let args: ImagegenArgs = serde_json::from_str(&arguments).unwrap();
    assert_eq!(
        args.referenced_image_paths
            .as_ref()
            .expect("the native reference path must be present")
            .len(),
        1
    );
    assert_eq!(
        image_history_requirement(&ToolPayload::Function { arguments }),
        ConversationHistoryRequirement::None
    );
    for arguments in [
        "{".to_string(),
        serde_json::json!({"num_last_images_to_include": 1}).to_string(),
        serde_json::json!({"prompt": "edit", "unknown": true, "num_last_images_to_include": 1})
            .to_string(),
    ] {
        assert_eq!(
            image_history_requirement(&ToolPayload::Function { arguments }),
            ConversationHistoryRequirement::None
        );
    }
}

#[test]
fn uses_reserved_image_gen_namespace() {
    let ToolSpec::Namespace(spec) = imagegen_tool_spec() else {
        panic!("imagegen should advertise a namespace tool");
    };
    assert_eq!(spec.name, "image_gen");
    let ResponsesApiNamespaceTool::Function(function) = &spec.tools[0];
    assert_eq!(function.name, "imagegen");
    assert!(!function.strict);
    // Bounds and annotations added by Rust schema generation must not change
    // the reserved contract and make unrelated agent requests fail at startup.
    assert_eq!(
        serde_json::to_value(&function.parameters).unwrap(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" },
                "referenced_image_paths": {
                    "type": ["array", "null"],
                    "items": { "type": "string" }
                },
                "num_last_images_to_include": { "type": ["integer", "null"] }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    );
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
            referenced_image_paths: Some(vec!["/tmp/image.png".into()]),
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
                    .map(|index| format!("/tmp/image-{index}.png"))
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
            referenced_image_paths: Some(vec!["/tmp/image.png".into()]),
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
fn generated_output_preserves_image_and_hint_across_delivery_surfaces() {
    let saved_hint =
        extension_image_generation_output_hint("/tmp", "/tmp/call-1.png").expect("hint should fit");
    let fallback_hint = "If you need to use a generated image at another path, copy it and leave the original in place unless the user explicitly asks you to delete it.";
    let oversized_hint = extension_image_generation_output_hint("/tmp", "x".repeat(1024));
    assert_eq!(oversized_hint.as_deref(), Some(fallback_hint));

    for hint in [
        Some(saved_hint),
        Some("generated image save hint".to_string()),
        oversized_hint,
        None,
    ] {
        let output = GeneratedImageOutput {
            result: RESULT.to_string(),
            output_hint: hint.clone(),
        };
        let image_url = format!("data:image/png;base64,{RESULT}");
        let mut expected_json = serde_json::json!({"image_url": image_url});
        let mut content = vec![FunctionCallOutputContentItem::InputImage {
            image_url,
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }];
        if let Some(hint) = hint {
            expected_json["output_hint"] = hint.clone().into();
            content.push(FunctionCallOutputContentItem::InputText { text: hint });
        }
        assert_eq!(output.code_mode_result(&function_payload()), expected_json);
        assert_eq!(
            output.to_response_item("call-1", &function_payload()),
            ResponseInputItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::ContentItems(content),
                    success: Some(true),
                },
            }
        );
        assert_eq!(output.log_preview(), "[generated image]");
        assert!(output.success_for_logging());
    }
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
struct ReferenceImageFileSystem {
    expected: codex_utils_path_uri::PathUri,
    reads: std::sync::atomic::AtomicUsize,
}

impl codex_exec_server::ExecutorFileSystem for ReferenceImageFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, codex_utils_path_uri::PathUri> {
        Box::pin(async { panic!("unexpected filesystem operation: canonicalize") })
    }
    fn read_file<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async { panic!("unexpected filesystem operation: read_file") })
    }
    fn read_file_stream<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, codex_exec_server::FileSystemReadStream>
    {
        Box::pin(async { panic!("unexpected filesystem operation: read_file_stream") })
    }
    fn write_file<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { panic!("unexpected filesystem operation: write_file") })
    }
    fn create_directory<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _options: codex_exec_server::CreateDirectoryOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { panic!("unexpected filesystem operation: create_directory") })
    }
    fn get_metadata<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, codex_exec_server::FileMetadata> {
        Box::pin(async { panic!("unexpected filesystem operation: get_metadata") })
    }
    fn read_directory<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, Vec<codex_exec_server::ReadDirectoryEntry>>
    {
        Box::pin(async { panic!("unexpected filesystem operation: read_directory") })
    }
    fn remove<'a>(
        &'a self,
        _path: &'a codex_utils_path_uri::PathUri,
        _options: codex_exec_server::RemoveOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { panic!("unexpected filesystem operation: remove") })
    }
    fn copy<'a>(
        &'a self,
        _source: &'a codex_utils_path_uri::PathUri,
        _destination: &'a codex_utils_path_uri::PathUri,
        _options: codex_exec_server::CopyOptions,
        _sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { panic!("unexpected copy") })
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a codex_utils_path_uri::PathUri,
        limit: usize,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            use base64::Engine;
            assert_eq!(path, &self.expected);
            assert_eq!(limit, super::MAX_PROMPT_IMAGE_SOURCE_BYTES);
            assert!(
                sandbox.is_some(),
                "selected environment sandbox must be retained"
            );
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Some(super::BASE64_STANDARD.decode(VALID_PNG).unwrap()))
        })
    }
}

#[tokio::test]
async fn reference_paths_use_primary_filesystem_conventions_on_every_host() {
    use codex_utils_path_uri::PathUri;
    for (cwd, path, expected) in [
        (
            "file:///C:/workspace",
            r"C:\images\reference.png",
            "file:///C:/images/reference.png",
        ),
        (
            "file:///C:/workspace",
            r"\\server\share\reference.png",
            "file://server/share/reference.png",
        ),
        (
            "file:///workspace",
            "/images/reference.png",
            "file:///images/reference.png",
        ),
        (
            "file:///workspace",
            "reference.png",
            "file:///workspace/reference.png",
        ),
    ] {
        let args: ImagegenArgs = serde_json::from_value(serde_json::json!({
            "prompt":"edit", "referenced_image_paths":[path]
        }))
        .unwrap();
        let fs = std::sync::Arc::new(ReferenceImageFileSystem {
            expected: PathUri::parse(expected).unwrap(),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let cwd = PathUri::parse(cwd).unwrap();
        let environment = codex_extension_api::ToolEnvironment {
            environment_id: "primary".into(),
            cwd: cwd.clone(),
            file_system: fs.clone(),
            // This filesystem can be foreign to the test host. Legacy policy
            // conversion deliberately resolves native paths on the receiving host.
            file_system_sandbox_context:
                codex_exec_server::FileSystemSandboxContext::from_permission_profile_with_cwd(
                    codex_protocol::models::PermissionProfile::Disabled,
                    cwd,
                ),
        };
        let result = request_for_call_args(&args, &[], Some("primary"), &[environment])
            .await
            .unwrap();
        let ImageRequest::Edit(request) = result else {
            panic!("edit");
        };
        assert_eq!(request.images.len(), 1);
        assert_eq!(
            request.images[0].image_url,
            format!("data:image/png;base64,{VALID_PNG}")
        );
        assert_eq!(fs.reads.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
