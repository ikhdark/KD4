use std::io::Cursor;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

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
        (Some(ImageDetail::High), (2048, 2048), (1600, 1600)),
        (Some(ImageDetail::Original), (6401, 100), (6000, 94)),
        (Some(ImageDetail::Original), (3201, 3201), (3200, 3200)),
        (Some(ImageDetail::Auto), (2048, 2048), (1600, 1600)),
        (None, (2048, 2048), (1600, 1600)),
    ] {
        let (image_url, _) = png_data_url(input_dimensions.0, input_dimensions.1);
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
        let [ContentItem::InputImage { image_url, .. }] = content.as_slice() else {
            panic!("expected image");
        };
        assert_eq!(decoded_image(image_url).1.dimensions(), expected_dimensions);
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
                    FunctionCallOutputContentItem::InputText {
                        text: UNSUPPORTED_LOW_DETAIL_PLACEHOLDER.to_string(),
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
            ImagePreparationError::UnsupportedLowDetail,
            UNSUPPORTED_LOW_DETAIL_PLACEHOLDER,
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

fn mixed_owned_preparation_items() -> Vec<ResponseItem> {
    let (resized_image_url, _) = png_data_url(/*width*/ 2048, /*height*/ 2048);
    let (small_image_url, _) = png_data_url(/*width*/ 64, /*height*/ 32);
    vec![
        ResponseItem::Message {
            id: Some("message-1".to_string()),
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "before".to_string(),
                },
                ContentItem::InputImage {
                    image_url: resized_image_url,
                    detail: Some(ImageDetail::High),
                },
                ContentItem::InputImage {
                    image_url: "https://example.com/remote.png".to_string(),
                    detail: Some(ImageDetail::High),
                },
                ContentItem::InputText {
                    text: "after".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: Some("tool-output-1".to_string()),
            call_id: "call-1".to_string(),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "tool-before".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: small_image_url,
                        detail: Some(ImageDetail::Low),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: "tool-after".to_string(),
                    },
                ]),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Other,
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn owned_preparation_matches_synchronous_bytes_placeholders_and_order() {
    let items = mixed_owned_preparation_items();
    let mut expected = items.clone();
    prepare_response_items(&mut expected);

    let actual = prepare_response_items_owned(items)
        .await
        .expect("blocking image preparation");

    assert_eq!(actual, expected);
}

#[test]
fn owned_preparation_keeps_the_async_executor_responsive() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let (blocker_started_tx, blocker_started_rx) = mpsc::channel();
        let (release_blocker_tx, release_blocker_rx) = mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            blocker_started_tx.send(()).expect("blocker started");
            release_blocker_rx.recv().expect("release blocker");
        });
        blocker_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking worker started");

        let preparation = tokio::spawn(prepare_response_items_owned(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "https://example.com/remote.png".to_string(),
                detail: Some(ImageDetail::High),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }]));
        tokio::task::yield_now().await;
        assert!(
            !preparation.is_finished(),
            "preparation must queue on the occupied blocking pool"
        );

        tokio::time::timeout(Duration::from_millis(100), async {
            tokio::task::yield_now().await;
        })
        .await
        .expect("executor heartbeat");

        release_blocker_tx.send(()).expect("release blocker");
        blocker.await.expect("blocker task");
        let prepared = preparation
            .await
            .expect("preparation task")
            .expect("blocking image preparation");
        let ResponseItem::Message { content, .. } = &prepared[0] else {
            panic!("expected message");
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: REMOTE_IMAGE_URL_PLACEHOLDER.to_string(),
            }]
        );
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn panicking_blocking_preparation_resumes_the_panic() {
    let task = tokio::spawn(prepare_response_items_owned_with(
        vec![ResponseItem::Other],
        |_items| panic!("injected image preparation panic"),
    ));

    assert!(
        task.await
            .expect_err("awaiting task should resume the blocking panic")
            .is_panic()
    );
}

#[test]
fn cancelled_blocking_join_returns_the_join_error() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let (blocker_started_tx, blocker_started_rx) = mpsc::channel();
        let (release_blocker_tx, release_blocker_rx) = mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            blocker_started_tx.send(()).expect("blocker started");
            release_blocker_rx.recv().expect("release blocker");
        });
        blocker_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking worker started");

        let queued = tokio::task::spawn_blocking(|| vec![ResponseItem::Other]);
        queued.abort();
        release_blocker_tx.send(()).expect("release blocker");
        blocker.await.expect("blocker task");
        let error = await_response_items_preparation(queued)
            .await
            .expect_err("queued task should be cancelled");
        assert!(error.is_cancelled());
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_await_discards_a_partially_transformed_owned_vector() {
    let installed_history = Arc::new(Mutex::new(vec![ResponseItem::Message {
        id: Some("installed".to_string()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "unchanged".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]));
    let candidate = vec![ResponseItem::Other, ResponseItem::Other];
    let (partially_transformed_tx, partially_transformed_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (blocking_finished_tx, blocking_finished_rx) = mpsc::channel();
    let history_for_task = Arc::clone(&installed_history);
    let publication = tokio::spawn(async move {
        let prepared = prepare_response_items_owned_with(candidate, move |items| {
            items[0] = ResponseItem::Message {
                id: Some("partial".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "partial".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            };
            partially_transformed_tx
                .send(())
                .expect("partial transform observed");
            release_rx.recv().expect("release preparation");
            items[1] = ResponseItem::Other;
            blocking_finished_tx
                .send(())
                .expect("blocking preparation finished");
        })
        .await
        .expect("blocking preparation");
        *history_for_task.lock().expect("history lock") = prepared;
    });
    partially_transformed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("partial transform started");

    publication.abort();
    assert!(
        publication
            .await
            .expect_err("publication task should be cancelled")
            .is_cancelled()
    );
    release_tx.send(()).expect("release preparation");
    blocking_finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocking task completed after cancellation");

    assert_eq!(
        *installed_history.lock().expect("history lock"),
        vec![ResponseItem::Message {
            id: Some("installed".to_string()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "unchanged".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }]
    );
}
