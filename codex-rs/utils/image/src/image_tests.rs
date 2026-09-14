use std::io::Cursor;

use super::*;
use image::GenericImageView;
use image::ImageBuffer;
use image::ImageDecoder;
use image::Rgba;
use image::metadata::Orientation;

const TEST_RGB_ICC_PROFILE: &[u8] = b"0123456789abcdefRGB ";
const TEST_CMYK_ICC_PROFILE: &[u8] = b"0123456789abcdefCMYK";
const ROTATE_90_EXIF: &[u8] = &[
    0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn image_bytes(image: &ImageBuffer<Rgba<u8>, Vec<u8>>, format: ImageFormat) -> Vec<u8> {
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut encoded, format)
        .expect("encode image to bytes");
    encoded.into_inner()
}

fn image_bytes_with_metadata(
    image: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    format: ImageFormat,
    icc_profile: &[u8],
) -> Vec<u8> {
    let mut encoded = Vec::new();
    match format {
        ImageFormat::Png => {
            let mut encoder = PngEncoder::new(&mut encoded);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set PNG ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set PNG EXIF metadata");
            encoder
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .expect("encode PNG with metadata");
        }
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut encoded, 90);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set JPEG ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set JPEG EXIF metadata");
            encoder
                .encode_image(&DynamicImage::ImageRgba8(image.clone()))
                .expect("encode JPEG with metadata");
        }
        ImageFormat::WebP => {
            let mut encoder = WebPEncoder::new_lossless(&mut encoded);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set WebP ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set WebP EXIF metadata");
            encoder
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .expect("encode WebP with metadata");
        }
        _ => panic!("unsupported test format"),
    }
    encoded
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_original_image_when_within_bounds() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::WebP, "image/webp"),
    ] {
        let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
        let original_bytes = image_bytes(&image, format);

        let encoded = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes.clone(),
            PromptImageMode::ResizeToFit,
        )
        .expect("process image");

        assert_eq!(encoded.width, 64);
        assert_eq!(encoded.height, 32);
        assert_eq!(encoded.mime, mime);
        assert_eq!(encoded.bytes.as_ref(), original_bytes);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn current_thread_runtime_loads_prompt_image() {
    let image = ImageBuffer::from_pixel(48, 24, Rgba([11u8, 22, 33, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);

    for _ in 0..2 {
        let encoded = load_for_prompt_bytes(
            Path::new("current-thread-image.png"),
            original_bytes.clone(),
            PromptImageMode::Original,
        )
        .expect("process image on a current-thread runtime");

        assert_eq!((encoded.width, encoded.height), (48, 24));
        assert_eq!(encoded.mime, "image/png");
        assert_eq!(encoded.bytes.as_ref(), original_bytes);
    }
}

#[test]
fn downscales_large_image() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::WebP, "image/webp"),
    ] {
        let image = ImageBuffer::from_pixel(4096, 2048, Rgba([200u8, 10, 10, 255]));
        let original_bytes = image_bytes(&image, format);

        let processed = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process image");

        assert_eq!((processed.width, processed.height), (2048, 1024));
        assert_eq!(processed.mime, mime);

        let detected_format =
            image::guess_format(&processed.bytes).expect("detect resized output format");
        assert_eq!(detected_format, format);

        let loaded =
            image::load_from_memory(&processed.bytes).expect("read resized bytes back into image");
        assert_eq!(loaded.dimensions(), (processed.width, processed.height));
    }
}

#[test]
fn downscales_tall_image_to_fit_square_bounds() {
    let image = ImageBuffer::from_pixel(1024, 4096, Rgba([200u8, 10, 10, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes,
        PromptImageMode::ResizeToFit,
    )
    .expect("process image");

    assert_eq!(processed.width, 512);
    assert_eq!(processed.height, MAX_DIMENSION);
    assert_eq!(processed.mime, "image/png");
}

#[test]
fn resizing_preserves_supported_metadata() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let image = ImageBuffer::from_pixel(2050, 2, Rgba([200u8, 10, 10, 255]));
        let original_bytes = image_bytes_with_metadata(&image, format, TEST_RGB_ICC_PROFILE);

        let processed = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process image");

        assert_eq!((processed.width, processed.height), (2048, 2));

        let mut decoder = ImageReader::with_format(Cursor::new(&processed.bytes), format)
            .into_decoder()
            .expect("create decoder");
        assert_eq!(
            (
                decoder.dimensions(),
                decoder.orientation().expect("read orientation"),
                decoder.icc_profile().expect("read ICC profile"),
                decoder.exif_metadata().expect("read EXIF metadata"),
            ),
            (
                (2048, 2),
                Orientation::Rotate90,
                Some(TEST_RGB_ICC_PROFILE.to_vec()),
                Some(ROTATE_90_EXIF.to_vec()),
            )
        );
    }
}

#[test]
fn resizing_drops_non_rgb_icc_profile() {
    let image = ImageBuffer::from_pixel(2050, 2, Rgba([200u8, 10, 10, 255]));
    let original_bytes =
        image_bytes_with_metadata(&image, ImageFormat::Jpeg, TEST_CMYK_ICC_PROFILE);

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes,
        PromptImageMode::ResizeToFit,
    )
    .expect("process image");

    let mut decoder = ImageReader::with_format(Cursor::new(&processed.bytes), ImageFormat::Jpeg)
        .into_decoder()
        .expect("create decoder");
    assert_eq!(
        (
            decoder.icc_profile().expect("read ICC profile"),
            decoder.exif_metadata().expect("read EXIF metadata"),
        ),
        (None, Some(ROTATE_90_EXIF.to_vec()))
    );
}

#[test]
fn preserves_large_image_in_original_mode() {
    let image = ImageBuffer::from_pixel(4096, 2048, Rgba([180u8, 30, 30, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes.clone(),
        PromptImageMode::Original,
    )
    .expect("process image");

    assert_eq!(processed.width, 4096);
    assert_eq!(processed.height, 2048);
    assert_eq!(processed.mime, "image/png");
    assert_eq!(processed.bytes.as_ref(), original_bytes);
}

#[test]
fn data_url_processing_preserves_supported_source_bytes() {
    let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);
    let image_url = data_url_from_bytes("image/png", &original_bytes)
        .replacen("data:", "DATA:", 1)
        .replacen(";base64,", ";BASE64,", 1);

    let processed = load_data_url_for_prompt(&image_url, PromptImageMode::ResizeToFit)
        .expect("process data URL image");

    assert_eq!(processed.width, 64);
    assert_eq!(processed.height, 32);
    assert_eq!(processed.mime, "image/png");
    assert_eq!(processed.bytes.as_ref(), original_bytes);
}

#[test]
fn data_url_processing_converts_gif_to_png() {
    let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
    let gif_bytes = image_bytes(&image, ImageFormat::Gif);
    let image_url = data_url_from_bytes("image/gif", &gif_bytes);

    let processed = load_data_url_for_prompt(&image_url, PromptImageMode::ResizeToFit)
        .expect("process GIF data URL");

    assert_eq!(processed.mime, "image/png");
    assert_eq!(
        image::guess_format(&processed.bytes).expect("detect processed format"),
        ImageFormat::Png
    );
}

#[test]
fn data_url_processing_rejects_malformed_input() {
    for image_url in [
        "image/png;base64,AAAA",
        "data:image/png;base64",
        "data:image/png,AAAA",
        "data:image/png;base64,not base64",
    ] {
        assert!(matches!(
            load_data_url_for_prompt(image_url, PromptImageMode::ResizeToFit),
            Err(ImageProcessingError::InvalidDataUrl { .. })
        ));
    }
}

#[test]
fn resize_with_limits_respects_dimension_and_patch_budgets() {
    let image = ImageBuffer::from_pixel(2048, 2048, Rgba([200u8, 10, 10, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);
    let limits = PromptImageResizeLimits {
        max_dimension: 2048,
        max_patches: 2_500,
    };

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes,
        PromptImageMode::ResizeWithLimits(limits),
    )
    .expect("process image with explicit limits");

    assert_eq!((processed.width, processed.height), (1600, 1600));
}

#[test]
fn fails_cleanly_for_invalid_images() {
    let err = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        b"not an image".to_vec(),
        PromptImageMode::ResizeToFit,
    )
    .expect_err("invalid image should fail");
    assert!(matches!(
        err,
        ImageProcessingError::Decode { .. } | ImageProcessingError::UnsupportedImageFormat { .. }
    ));
}

#[test]
fn reprocesses_updated_file_contents() {
    let first_image = ImageBuffer::from_pixel(32, 16, Rgba([20u8, 120, 220, 255]));
    let first_bytes = image_bytes(&first_image, ImageFormat::Png);

    let first = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        first_bytes,
        PromptImageMode::ResizeToFit,
    )
    .expect("process first image");

    let second_image = ImageBuffer::from_pixel(96, 48, Rgba([50u8, 60, 70, 255]));
    let second_bytes = image_bytes(&second_image, ImageFormat::Png);

    let second = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        second_bytes,
        PromptImageMode::ResizeToFit,
    )
    .expect("process updated image");

    assert_eq!(first.width, 32);
    assert_eq!(first.height, 16);
    assert_eq!(second.width, 96);
    assert_eq!(second.height, 48);
    assert_ne!(second.bytes, first.bytes);
}

#[test]
fn bounds_cache_by_encoded_byte_size() {
    let cache = ImageCache::new(NonZeroUsize::new(4).expect("non-zero cache capacity"));
    let key = |digest_byte| ImageCacheKey {
        digest: [digest_byte; 20],
        mode: PromptImageMode::Original,
    };
    let image = |size| EncodedImage {
        bytes: vec![0; size].into(),
        mime: "image/png".to_string(),
        width: 1,
        height: 1,
    };

    cache_image(&cache, key(1), image(3), /*byte_capacity*/ 5);
    cache_image(&cache, key(2), image(3), /*byte_capacity*/ 5);
    cache_image(&cache, key(3), image(6), /*byte_capacity*/ 5);

    assert!(cache.get(&key(1)).is_none());
    assert!(cache.get(&key(2)).is_some());
    assert!(cache.get(&key(3)).is_none());
}

#[test]
fn narrow_images_use_the_available_patch_budget() {
    for (width, height, expected) in [(2048, 1, (1024, 1)), (1, 2048, (1, 1024))] {
        let input = ImageBuffer::from_pixel(width, height, Rgba([21u8, 34, 55, 255]));
        let result = load_for_prompt_bytes(
            Path::new("narrow.png"),
            image_bytes(&input, ImageFormat::Png),
            PromptImageMode::ResizeWithLimits(PromptImageResizeLimits {
                max_dimension: 2048,
                max_patches: 32,
            }),
        )
        .unwrap();
        assert_eq!((result.width, result.height), expected);
        assert_eq!(
            image::load_from_memory(&result.bytes).unwrap().dimensions(),
            expected
        );
    }
}

#[test]
fn zero_resize_limits_are_rejected_at_both_entrypoints() {
    let input = image_bytes(
        &ImageBuffer::from_pixel(2, 2, Rgba([21u8, 34, 55, 255])),
        ImageFormat::Png,
    );
    for limits in [
        PromptImageResizeLimits {
            max_dimension: 0,
            max_patches: 32,
        },
        PromptImageResizeLimits {
            max_dimension: 2048,
            max_patches: 0,
        },
    ] {
        let mode = PromptImageMode::ResizeWithLimits(limits);
        assert!(matches!(
            load_for_prompt_bytes(Path::new("zero.png"), input.clone(), mode),
            Err(ImageProcessingError::InvalidResizeLimits)
        ));
        assert!(matches!(
            load_data_url_for_prompt(&data_url_from_bytes("image/png", &input), mode),
            Err(ImageProcessingError::InvalidResizeLimits)
        ));
    }
}

#[test]
fn cache_reuses_buffers_and_separates_resize_modes() {
    let input = image_bytes(
        &ImageBuffer::from_pixel(32, 16, Rgba([19u8, 127, 243, 255])),
        ImageFormat::Png,
    );
    let process =
        |mode| load_for_prompt_bytes(Path::new("cache.png"), input.clone(), mode).unwrap();
    let first = process(PromptImageMode::Original);
    let second = process(PromptImageMode::Original);
    assert!(Arc::ptr_eq(&first.bytes, &second.bytes));
    let resized = process(PromptImageMode::ResizeWithLimits(PromptImageResizeLimits {
        max_dimension: 16,
        max_patches: 1,
    }));
    assert_eq!((first.width, first.height), (32, 16));
    assert_eq!((resized.width, resized.height), (16, 8));
    assert_ne!(first.bytes, resized.bytes);
}

#[test]
fn decode_errors_preserve_io_and_resource_failures() {
    for source in [
        image::ImageError::IoError(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "incomplete image",
        )),
        image::ImageError::Limits(image::error::LimitError::from_kind(
            image::error::LimitErrorKind::InsufficientMemory,
        )),
    ] {
        let expected = source.to_string();
        let error = ImageProcessingError::decode_error(Path::new("input.png"), source);
        assert!(!error.is_invalid_image());
        let ImageProcessingError::Decode { source, .. } = error else {
            panic!("lost decode cause")
        };
        assert_eq!(source.to_string(), expected);
    }
}

#[test]
fn decoded_image_size_is_limited_before_allocation() {
    let mut input = image_bytes(
        &ImageBuffer::from_pixel(1, 1, Rgba([1u8, 2, 3, 255])),
        ImageFormat::Gif,
    );
    // GIF logical-screen dimensions can exceed the small encoded frame dimensions.
    input[6..10].copy_from_slice(&[255, 255, 255, 255]);
    assert!(matches!(
        load_for_prompt_bytes(Path::new("large.gif"), input, PromptImageMode::Original),
        Err(ImageProcessingError::Decode {
            source: image::ImageError::Limits(_),
            ..
        })
    ));
}
