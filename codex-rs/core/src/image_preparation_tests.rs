use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_utils_image::data_url_from_bytes;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageBuffer;
use image::ImageFormat;
use image::Rgba;
use pretty_assertions::assert_eq;

use super::*;

fn png_data_url(width: u32, height: u32) -> (String, Vec<u8>) {
    let image = ImageBuffer::from_pixel(width, height, Rgba([10u8, 20, 30, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode PNG");
    let bytes = encoded.into_inner();
    (data_url_from_bytes("image/png", &bytes), bytes)
}

fn decoded_image(image_url: &str) -> (Vec<u8>, DynamicImage) {
    let (_, payload) = image_url.split_once(',').expect("data URL payload");
    let bytes = BASE64_STANDARD.decode(payload).expect("decode image URL");
    let image = image::load_from_memory(&bytes).expect("decode processed image");
    (bytes, image)
}

#[test]
fn preparation_preserves_small_image_bytes_and_replaces_remote_urls() {
    let (data_url, original_bytes) = png_data_url(/*width*/ 64, /*height*/ 32);
    let mut items = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputImage {
                image_url: data_url,
                detail: Some(ImageDetail::High),
            },
            ContentItem::InputImage {
                image_url: "https://example.com/image.png".to_string(),
                detail: Some(ImageDetail::Low),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    prepare_response_items(&mut items);

    let ResponseItem::Message { content, .. } = &items[0] else {
        panic!("expected message");
    };
    let [
        ContentItem::InputImage { image_url, .. },
        ContentItem::InputText { text },
    ] = content.as_slice()
    else {
        panic!("expected two images");
    };
    assert_eq!(decoded_image(image_url).0, original_bytes);
    assert_eq!(text, REMOTE_IMAGE_URL_PLACEHOLDER);
}

#[test]
fn detail_policies_apply_the_expected_budgets() {
    for (detail, input_dimensions, expected_dimensions) in [
        (Some(ImageDetail::Low), (2048, 2048), (512, 512)),
        (Some(ImageDetail::Low), (2048, 1024), (512, 256)),
        (Some(ImageDetail::Low), (64, 32), (64, 32)),
        (Some(ImageDetail::High), (2048, 2048), (1600, 1600)),
        (Some(ImageDetail::Original), (6401, 100), (6000, 94)),
        (Some(ImageDetail::Original), (3201, 3201), (3200, 3200)),
        (Some(ImageDetail::Auto), (2048, 2048), (1600, 1600)),
        (None, (2048, 2048), (1600, 1600)),
    ] {
        // Exercise dimension policy independently of the stricter RGBA working-
        // set limit: alpha-safe filtering needs additional full-size float buffers.
        let image = ImageBuffer::from_pixel(
            input_dimensions.0,
            input_dimensions.1,
            image::Rgb([10u8, 20, 30]),
        );
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let image_url = data_url_from_bytes("image/png", encoded.get_ref());
        let mut items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage { image_url, detail }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];

        prepare_response_items(&mut items);

        let ResponseItem::Message { content, .. } = &items[0] else {
            panic!("expected message");
        };
        let Some(ContentItem::InputImage {
            image_url,
            detail: prepared_detail,
        }) = content.first()
        else {
            panic!("expected image");
        };
        assert_eq!(decoded_image(image_url).1.dimensions(), expected_dimensions);
        assert_eq!(*prepared_detail, detail);
        let resized_original =
            detail == Some(ImageDetail::Original) && input_dimensions != expected_dimensions;
        assert_eq!(content.len(), if resized_original { 2 } else { 1 });
        if resized_original {
            let ContentItem::InputText { text } = &content[1] else {
                panic!("missing resize notice");
            };
            assert!(text.contains(&format!("{}x{}", input_dimensions.0, input_dimensions.1)));
            assert!(text.contains(&format!(
                "{}x{}",
                expected_dimensions.0, expected_dimensions.1
            )));
            assert!(text.contains("exact source resolution was not delivered"));
        }
    }
}

#[test]
fn preparation_replaces_only_failed_tool_images_and_preserves_metadata() {
    let (valid_image_url, _) = png_data_url(/*width*/ 64, /*height*/ 32);
    let expected_valid_image_url = valid_image_url.clone();
    let mut items = vec![ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "before".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,%%%".to_string(),
                    detail: Some(ImageDetail::High),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: data_url_from_bytes("image/png", b"not an image"),
                    detail: Some(ImageDetail::High),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: valid_image_url.clone(),
                    detail: Some(ImageDetail::Low),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: valid_image_url,
                    detail: Some(ImageDetail::High),
                },
            ]),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }];

    prepare_response_items(&mut items);

    assert_eq!(
        items,
        vec![ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "before".to_string(),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: IMAGE_PROCESSING_ERROR_PLACEHOLDER.to_string(),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: IMAGE_PROCESSING_ERROR_PLACEHOLDER.to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: expected_valid_image_url.clone(),
                        detail: Some(ImageDetail::Low),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: expected_valid_image_url,
                        detail: Some(ImageDetail::High),
                    },
                ]),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }]
    );
}

#[test]
fn preparation_errors_use_bounded_actionable_placeholders() {
    let cases = [
        (
            ImagePreparationError::RemoteUrlUnsupported,
            REMOTE_IMAGE_URL_PLACEHOLDER,
        ),
        (
            ImagePreparationError::Processing(ImageProcessingError::ImageTooLarge {
                representation: "decoded input",
                size: 2,
                max: 1,
            }),
            IMAGE_TOO_LARGE_PLACEHOLDER,
        ),
        (
            ImagePreparationError::Processing(ImageProcessingError::InvalidDataUrl {
                reason: "details remain in logs".to_string(),
            }),
            IMAGE_PROCESSING_ERROR_PLACEHOLDER,
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.placeholder(), expected);
    }
}

#[test]
fn final_detail_negotiation_is_reversible_and_applies_to_user_and_tool_images() {
    let (image_url, _) = png_data_url(2200, 2);
    let tool_output = FunctionCallOutputPayload::from_content_items(vec![
        FunctionCallOutputContentItem::InputText {
            text: "before".into(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: image_url.clone(),
            detail: Some(ImageDetail::Original),
        },
        FunctionCallOutputContentItem::InputText {
            text: "after".into(),
        },
    ]);
    let canonical: std::sync::Arc<[ResponseItem]> = vec![
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputImage {
                image_url,
                detail: Some(ImageDetail::Original),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "function".into(),
            output: tool_output.clone(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "custom".into(),
            name: None,
            output: tool_output,
            internal_chat_message_metadata_passthrough: None,
        },
    ]
    .into();
    let supported = for_model_image_detail(canonical.clone(), true);
    assert!(std::sync::Arc::ptr_eq(&supported, &canonical));
    let downgraded = for_model_image_detail(canonical.clone(), false);
    let json = serde_json::to_string(&downgraded).unwrap();
    assert!(!json.contains("\"detail\":\"original\""));
    assert_eq!(
        json.matches("Original image detail is unavailable").count(),
        3
    );
    let ResponseItem::Message { content, .. } = &downgraded[0] else {
        panic!("user message");
    };
    let ContentItem::InputImage { image_url, detail } = &content[0] else {
        panic!("image");
    };
    assert_eq!(*detail, Some(ImageDetail::High));
    assert_eq!(decoded_image(image_url).1.dimensions(), (2048, 2));
    assert!(std::sync::Arc::ptr_eq(
        &for_model_image_detail(canonical.clone(), true),
        &canonical
    ));
    assert_eq!(
        for_model_image_detail(downgraded.clone(), false),
        downgraded
    );
}

#[test]
fn animation_notice_stays_next_to_its_static_preview_and_is_idempotent() {
    let mut gif = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
        for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    3,
                    2,
                    Rgba(color),
                )))
                .unwrap();
        }
    }
    let mut items = vec![ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![
            ContentItem::InputText {
                text: "before".into(),
            },
            ContentItem::InputImage {
                image_url: data_url_from_bytes("image/gif", &gif),
                detail: None,
            },
            ContentItem::InputText {
                text: "after".into(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    prepare_response_items(&mut items);
    let ResponseItem::Message { content, .. } = &items[0] else {
        panic!("message");
    };
    assert_eq!(content.len(), 4);
    assert!(matches!(&content[1], ContentItem::InputImage { .. }));
    assert!(
        matches!(&content[2], ContentItem::InputText { text } if text.contains("other animation frames have not been analyzed"))
    );
    let prepared = items.clone();
    prepare_response_items(&mut items);
    assert_eq!(items, prepared);
}

#[test]
fn unsupported_representations_are_isolated_in_every_history_shape() {
    for url in [
        "file:///tmp/a.png",
        "ftp://host/a.png",
        "blob:id",
        "",
        "plain",
        " https://host/a.png",
    ] {
        let content = vec![
            ContentItem::InputText {
                text: "before".into(),
            },
            ContentItem::InputImage {
                image_url: url.into(),
                detail: Some(ImageDetail::Low),
            },
            ContentItem::InputText {
                text: "after".into(),
            },
        ];
        let output = FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "before".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: url.into(),
                detail: Some(ImageDetail::Low),
            },
            FunctionCallOutputContentItem::InputText {
                text: "after".into(),
            },
        ]);
        let mut items = vec![
            ResponseItem::Message {
                id: None,
                role: "user".into(),
                content,
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                internal_chat_message_metadata_passthrough: None,
                call_id: "function".into(),
                output: output.clone(),
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "custom".into(),
                name: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        prepare_response_items(&mut items);
        for item in &items {
            let serialized = serde_json::to_string(item).unwrap();
            assert!(!serialized.contains("input_image"));
            assert!(serialized.contains("before") && serialized.contains("after"));
            assert!(serialized.contains(UNSUPPORTED_IMAGE_URL_PLACEHOLDER));
        }
    }
}

#[tokio::test]
async fn local_image_acquisition_rejects_oversize_and_non_files_before_conversion() {
    use codex_protocol::models::LocalImagePreparation;
    use codex_protocol::models::ResponseInputItem;
    use codex_protocol::user_input::UserInput;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("oversize.png");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 + 1)
        .unwrap();
    let (session, _) = crate::session::tests::make_session_and_context().await;
    for path in [path, temp.path().to_path_buf()] {
        let input = vec![
            UserInput::Text {
                text: "keep instructions".into(),
                text_elements: Vec::new(),
            },
            UserInput::LocalImage {
                path,
                detail: Some(ImageDetail::Low),
            },
        ];
        for mode in [LocalImagePreparation::Process, LocalImagePreparation::Defer] {
            let item = ResponseInputItem::from_user_input(input.clone(), mode);
            let json = serde_json::to_string(&item).unwrap();
            assert!(!json.contains("input_image"));
            assert!(
                json.contains("keep instructions")
                    && json.contains("could not read the local image")
            );
        }
        let item = session.response_item_from_user_input(input).await.unwrap();
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("input_image"));
        assert!(
            json.contains("keep instructions") && json.contains("could not read the local image")
        );
    }
}
