use std::borrow::Cow;
use std::io::Cursor;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_utils_cache::BlockingLruCache;
use codex_utils_cache::sha1_digest;
use image::DynamicImage;
use image::ImageDecoder;
use image::ImageEncoder;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;

const DATA_URL_PREFIX: &str = "data:";
pub const PROMPT_IMAGE_PATCH_SIZE: u32 = 32;
/// Maximum width or height used when resizing images before uploading.
pub const MAX_DIMENSION: u32 = 2048;
/// Maximum accepted byte length for prompt image input representations.
///
/// This is a high sanity guard against pathological inputs, not a protocol
/// requirement or target upload size.
pub const MAX_PROMPT_IMAGE_INPUT_BYTES: usize = 1024 * 1024 * 1024;
/// Maximum source byte length accepted by local tools before prompt-image processing.
///
/// Source bytes can coexist with a base64 representation and downstream copies,
/// so this memory-safety bound is intentionally tighter than the high sanity
/// guard for already-materialized prompt input representations.
pub const MAX_PROMPT_IMAGE_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_CACHE_BYTES: usize = MAX_PROMPT_IMAGE_SOURCE_BYTES;
const MAX_IMAGE_PROCESSING_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMAGE_METADATA_BYTES: u64 = 16 * 1024 * 1024;

pub mod error;

pub use crate::error::ImageProcessingError;

/// Bounded local acquisition for synchronous protocol/clipboard callers. Core
/// sessions use ExecutorFileSystem's cancellable stable-snapshot reader instead.
pub fn read_image_source(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let inspect = |metadata: &std::fs::Metadata| {
        if !metadata.is_file() || metadata.len() > MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "image source must be a regular file no larger than 64 MiB",
            ))
        } else {
            Ok(())
        }
    };
    let metadata = std::fs::metadata(path)?;
    inspect(&metadata)?;
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    inspect(&before)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > MAX_PROMPT_IMAGE_SOURCE_BYTES
        || before.len() != bytes.len() as u64
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "image source changed or exceeded its size limit",
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub bytes: Arc<[u8]>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_dimensions: (u32, u32),
    /// A static decoder does not establish the contents of later frames.
    pub animation_present: bool,
}

impl EncodedImage {
    pub fn into_data_url(self) -> String {
        data_url_from_bytes(self.mime, &self.bytes)
    }
}

/// Wraps image bytes in a data URL without decoding or validating them.
pub fn data_url_from_bytes(mime: &str, bytes: &[u8]) -> String {
    const BASE64_MARKER: &str = ";base64,";
    let prefix_len = DATA_URL_PREFIX.len() + mime.len() + BASE64_MARKER.len();
    // Reserve the full payload up front; multi-megabyte images otherwise
    // regrow and copy the string repeatedly while encoding.
    let payload_len = base64::encoded_len(bytes.len(), /*padding*/ true).unwrap_or(0);
    let mut url = String::with_capacity(prefix_len.saturating_add(payload_len));
    url.push_str(DATA_URL_PREFIX);
    url.push_str(mime);
    url.push_str(BASE64_MARKER);
    BASE64_STANDARD.encode_string(bytes, &mut url);
    url
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptImageMode {
    ResizeToFit,
    Original,
    ResizeWithLimits(PromptImageResizeLimits),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptImageResizeLimits {
    pub max_dimension: u32,
    pub max_patches: usize,
}

struct ImageMetadata {
    icc_profile: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
    png_color_chunks: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageCacheKey {
    digest: [u8; 20],
    mode: PromptImageMode,
}

type ImageCache = BlockingLruCache<ImageCacheKey, EncodedImage>;

static IMAGE_CACHE: LazyLock<ImageCache> =
    LazyLock::new(|| BlockingLruCache::new(NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN)));

pub fn load_for_prompt_bytes(
    path: &Path,
    file_bytes: Vec<u8>,
    mode: PromptImageMode,
) -> Result<EncodedImage, ImageProcessingError> {
    if file_bytes.len() > MAX_PROMPT_IMAGE_SOURCE_BYTES {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "source",
            size: file_bytes.len(),
            max: MAX_PROMPT_IMAGE_SOURCE_BYTES,
        });
    }
    if let PromptImageMode::ResizeWithLimits(limits) = mode
        && (limits.max_dimension == 0 || limits.max_patches == 0)
    {
        return Err(ImageProcessingError::InvalidResizeLimits);
    }
    let path_buf = path.to_path_buf();

    let key = ImageCacheKey {
        digest: sha1_digest(&file_bytes),
        mode,
    };

    if let Some(image) = IMAGE_CACHE.get(&key) {
        return Ok(image);
    }

    let image = (move || {
        let guessed_format = image::guess_format(&file_bytes)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        check_png_icc_budget(&file_bytes)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        let format = match guessed_format {
            ImageFormat::Png => Some(ImageFormat::Png),
            ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
            ImageFormat::Gif => Some(ImageFormat::Gif),
            ImageFormat::WebP => Some(ImageFormat::WebP),
            _ => None,
        };
        let animation_present = image_has_animation(&file_bytes, guessed_format);

        let mut reader = ImageReader::with_format(Cursor::new(&file_bytes), guessed_format);
        // PNG expands compressed metadata during construction, before set_limits.
        let mut header_limits = image::Limits::default();
        header_limits.max_alloc = Some(MAX_IMAGE_METADATA_BYTES);
        reader.limits(header_limits);
        let mut decoder = reader
            .into_decoder()
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        // Match ImageReader::decode: into_decoder alone does not reserve the
        // decoded pixel buffer against the allocation limit.
        let mut decode_limits = image::Limits::default();
        decode_limits
            .reserve(decoder.total_bytes())
            .and_then(|()| decoder.set_limits(decode_limits))
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        let (width, height) = decoder.dimensions();
        let source_dimensions = (width, height);
        let target = match mode {
            PromptImageMode::ResizeToFit => prompt_image_output_dimensions_for_limits(
                width,
                height,
                PromptImageResizeLimits {
                    max_dimension: MAX_DIMENSION,
                    max_patches: usize::MAX,
                },
            ),
            PromptImageMode::ResizeWithLimits(limits) => {
                prompt_image_output_dimensions_for_limits(width, height, limits)
            }
            PromptImageMode::Original => (width, height),
        };
        let transform = target != (width, height) || !format.is_some_and(can_preserve_source_bytes);
        let decoded_bytes = decoder.total_bytes();
        let color = decoder.color_type();
        let source_bytes = file_bytes.len();
        check_processing_budget(
            decoded_bytes,
            color,
            (width, height),
            target,
            source_bytes,
            transform,
            MAX_IMAGE_PROCESSING_BYTES,
        )?;
        // Preserve the metadata most important for rendering prompt images faithfully: the color
        // profile and EXIF data, including orientation. Other format-specific metadata is
        // intentionally not copied.
        let metadata = if transform {
            ImageMetadata {
                // Only RGB profiles are safe across every re-encoding path. For example, JPEG decoding
                // can convert CMYK/YCCK pixels to RGB while retaining the source profile; copying it
                // would mislabel the output. Bytes 16..20 are the ICC data color space signature.
                icc_profile: decoder
                    .icc_profile()
                    .ok()
                    .flatten()
                    .filter(|profile| profile.get(16..20) == Some(b"RGB ")),
                exif: decoder.exif_metadata().ok().flatten(),
                png_color_chunks: if guessed_format == ImageFormat::Png {
                    png_color_chunks(&file_bytes)
                } else {
                    Vec::new()
                },
            }
        } else {
            ImageMetadata {
                icc_profile: None,
                exif: None,
                png_color_chunks: Vec::new(),
            }
        };
        let metadata_bytes = metadata
            .icc_profile
            .as_ref()
            .map_or(0, Vec::len)
            .saturating_add(metadata.exif.as_ref().map_or(0, Vec::len));
        if metadata_bytes as u64 > MAX_IMAGE_METADATA_BYTES {
            return Err(ImageProcessingError::ImageTooLarge {
                representation: "metadata",
                size: metadata_bytes,
                max: MAX_IMAGE_METADATA_BYTES as usize,
            });
        }
        let dynamic = DynamicImage::from_decoder(decoder)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;

        let target_dimensions = if target != (width, height) {
            let opaque = is_opaque(&dynamic);
            if !opaque {
                // Only translucent pixels take the premultiplied f32 path; decoding
                // has now shown that this image needs those copies.
                check_processing_budget(
                    decoded_bytes
                        .saturating_add(premultiplied_alpha_scratch((width, height), target)),
                    color,
                    (width, height),
                    target,
                    source_bytes,
                    transform,
                    MAX_IMAGE_PROCESSING_BYTES,
                )?;
            }
            Some((
                target.0,
                target.1,
                resize_with_alpha(&dynamic, opaque, target.0, target.1),
            ))
        } else {
            None
        };

        let encoded = if let Some((width, height, resized)) = target_dimensions {
            let target_format = format
                .filter(|format| can_preserve_source_bytes(*format))
                .unwrap_or(ImageFormat::Png);
            let (bytes, output_format) = encode_image(&resized, target_format, metadata)?;
            let mime = format_to_mime(output_format);
            EncodedImage {
                bytes: bytes.into(),
                mime,
                width,
                height,
                source_dimensions,
                animation_present,
            }
        } else {
            if let Some(format) = format.filter(|format| can_preserve_source_bytes(*format)) {
                let mime = format_to_mime(format);
                EncodedImage {
                    bytes: file_bytes.into(),
                    mime,
                    width,
                    height,
                    source_dimensions,
                    animation_present,
                }
            } else {
                let (bytes, output_format) = encode_image(&dynamic, ImageFormat::Png, metadata)?;
                let mime = format_to_mime(output_format);
                EncodedImage {
                    bytes: bytes.into(),
                    mime,
                    width,
                    height,
                    source_dimensions,
                    animation_present,
                }
            }
        };

        Ok(encoded)
    })()?;

    cache_image(&IMAGE_CACHE, key, image.clone(), MAX_IMAGE_CACHE_BYTES);
    Ok(image)
}

// Account for simultaneously live named buffers, not just decoded pixels.
// image 0.25's vertical pass always uses source_width * target_height RGBA f32.
// `live_bytes` is the decoded image plus any scratch specific to its resize path.
fn check_processing_budget(
    live_bytes: u64,
    color: image::ColorType,
    source: (u32, u32),
    target: (u32, u32),
    source_bytes: usize,
    transform: bool,
    max_bytes: u64,
) -> Result<(), ImageProcessingError> {
    let estimate = (|| {
        let pixels = u64::from(target.0).checked_mul(u64::from(target.1))?;
        let output = pixels.checked_mul(u64::from(color.bytes_per_pixel()))?;
        let mut total = live_bytes
            .checked_add(source_bytes as u64)?
            .checked_add(MAX_IMAGE_METADATA_BYTES.checked_mul(2)?)?;
        if source != target {
            total = total
                .checked_add(
                    u64::from(source.0)
                        .checked_mul(u64::from(target.1))?
                        .checked_mul(16)?,
                )?
                .checked_add(output)?;
        }
        if transform {
            total = total.checked_add(output.checked_mul(2)?)?;
        }
        Some(total)
    })()
    .unwrap_or(u64::MAX);
    if estimate > max_bytes {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "processing working set",
            size: usize::try_from(estimate).unwrap_or(usize::MAX),
            max: usize::try_from(max_bytes).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

// Premultiplied resizing keeps RGBA f32 copies of the source and the target.
fn premultiplied_alpha_scratch(source: (u32, u32), target: (u32, u32)) -> u64 {
    (|| {
        u64::from(source.0)
            .checked_mul(u64::from(source.1))?
            .checked_mul(16)?
            .checked_add(
                u64::from(target.0)
                    .checked_mul(u64::from(target.1))?
                    .checked_mul(16)?,
            )
    })()
    .unwrap_or(u64::MAX)
}

fn is_opaque(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgba8(p) => p.pixels().all(|p| p[3] == u8::MAX),
        DynamicImage::ImageLumaA8(p) => p.pixels().all(|p| p[1] == u8::MAX),
        DynamicImage::ImageRgba16(p) => p.pixels().all(|p| p[3] == u16::MAX),
        DynamicImage::ImageLumaA16(p) => p.pixels().all(|p| p[1] == u16::MAX),
        DynamicImage::ImageRgba32F(p) => p.pixels().all(|p| p[3] == 1.0),
        _ => !image.color().has_alpha(),
    }
}

fn resize_with_alpha(image: &DynamicImage, opaque: bool, width: u32, height: u32) -> DynamicImage {
    if opaque {
        return image.resize_exact(width, height, FilterType::Triangle);
    }
    let mut premultiplied = image.to_rgba32f();
    for p in premultiplied.pixels_mut() {
        for channel in 0..3 {
            p[channel] *= p[3];
        }
    }
    let mut resized = image::imageops::resize(&premultiplied, width, height, FilterType::Triangle);
    for p in resized.pixels_mut() {
        for channel in 0..3 {
            p[channel] = if p[3] > 0.0 { p[channel] / p[3] } else { 0.0 };
        }
    }
    let resized = DynamicImage::ImageRgba32F(resized);
    if image.color().has_color() {
        resized.to_rgba8().into()
    } else {
        resized.to_luma_alpha8().into()
    }
}

/// Dimension-only probes share the constructor metadata bound with preparation.
pub fn prompt_image_dimensions(bytes: &[u8]) -> image::ImageResult<(u32, u32)> {
    check_png_icc_budget(bytes)?;
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_METADATA_BYTES);
    reader.limits(limits);
    reader.into_dimensions()
}

pub fn prompt_image_dimensions_from_path(path: &Path) -> image::ImageResult<(u32, u32)> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 {
        return Err(image::ImageError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "image source must be a regular file no larger than 64 MiB",
        )));
    }
    let file = std::fs::File::open(path)?;
    let mut reader = ImageReader::new(std::io::BufReader::new(file)).with_guessed_format()?;
    if reader.format() == Some(ImageFormat::Png) {
        // Inspect the bounded source, not just its header: png intentionally
        // suppresses ICC decompression-limit errors as optional metadata damage.
        return prompt_image_dimensions(&read_image_source(path)?);
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_METADATA_BYTES);
    reader.limits(limits);
    reader.into_dimensions()
}

fn cache_image(cache: &ImageCache, key: ImageCacheKey, image: EncodedImage, byte_capacity: usize) {
    if image.bytes.len() > byte_capacity {
        return;
    }

    // Image caching is optional when the cache is contended.
    let _ = cache.with_mut(|cache| {
        cache.put(key, image);
        let mut cached_bytes = cache
            .iter()
            .map(|(_, image)| image.bytes.len())
            .sum::<usize>();
        while cached_bytes > byte_capacity {
            let Some((_, evicted)) = cache.pop_lru() else {
                break;
            };
            cached_bytes -= evicted.bytes.len();
        }
    });
}

pub fn load_data_url_for_prompt(
    image_url: &str,
    mode: PromptImageMode,
) -> Result<EncodedImage, ImageProcessingError> {
    let rest = image_url
        .get(..DATA_URL_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX))
        .and_then(|_| image_url.get(DATA_URL_PREFIX.len()..))
        .ok_or_else(|| ImageProcessingError::InvalidDataUrl {
            reason: "missing data: prefix".to_string(),
        })?;
    let (metadata, encoded) =
        rest.split_once(',')
            .ok_or_else(|| ImageProcessingError::InvalidDataUrl {
                reason: "missing comma separator".to_string(),
            })?;
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err(ImageProcessingError::InvalidDataUrl {
            reason: "only base64 data URLs are supported".to_string(),
        });
    }

    let source_payload_limit =
        base64::encoded_len(MAX_PROMPT_IMAGE_SOURCE_BYTES, true).unwrap_or(usize::MAX);
    if encoded.len() > source_payload_limit {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "base64 payload",
            size: encoded.len(),
            max: source_payload_limit,
        });
    }
    let file_bytes =
        BASE64_STANDARD
            .decode(encoded)
            .map_err(|source| ImageProcessingError::InvalidDataUrl {
                reason: format!("invalid base64 payload: {source}"),
            })?;
    if file_bytes.len() > MAX_PROMPT_IMAGE_INPUT_BYTES {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "decoded input",
            size: file_bytes.len(),
            max: MAX_PROMPT_IMAGE_INPUT_BYTES,
        });
    }

    load_for_prompt_bytes(Path::new("<data-url-image>"), file_bytes, mode)
}

fn prompt_image_output_dimensions_for_limits(
    width: u32,
    height: u32,
    limits: PromptImageResizeLimits,
) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    if prompt_image_dimensions_fit(width, height, limits) {
        return (width, height);
    }

    let max_dimension_scale =
        (f64::from(limits.max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if prompt_image_dimensions_fit(width, height, limits) {
        return (width, height);
    }

    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let patch_size = f64::from(PROMPT_IMAGE_PATCH_SIZE);
    let mut scale =
        (patch_size * patch_size * limits.max_patches as f64 / width_f64 / height_f64).sqrt();
    // Match Responses patch-budget math: shrink by area, then round the scaled
    // patch grid down so integer output dimensions remain within the budget.
    let scaled_patches_wide = width_f64 * scale / patch_size;
    let scaled_patches_high = height_f64 * scale / patch_size;
    if scaled_patches_wide < 1.0 || scaled_patches_high < 1.0 {
        // A sub-patch axis still costs one patch. Fit the longest axis with
        // integer search instead of flooring that axis to zero and collapsing
        // both dimensions. The search never enlarges either dimension.
        let longest = width.max(height);
        let dimensions = |length: u32| {
            (
                (u64::from(width) * u64::from(length) / u64::from(longest)).max(1) as u32,
                (u64::from(height) * u64::from(length) / u64::from(longest)).max(1) as u32,
            )
        };
        let (mut low, mut high) = (1, longest);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let (w, h) = dimensions(middle);
            if prompt_image_dimensions_fit(w, h, limits) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        return dimensions(low);
    }
    scale *= (scaled_patches_wide.floor() / scaled_patches_wide)
        .min(scaled_patches_high.floor() / scaled_patches_high);

    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn prompt_image_dimensions_fit(width: u32, height: u32, limits: PromptImageResizeLimits) -> bool {
    let patches_wide = width.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patches_high = height.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patch_count = u64::from(patches_wide) * u64::from(patches_high);
    width <= limits.max_dimension
        && height <= limits.max_dimension
        && patch_count <= limits.max_patches as u64
}

fn can_preserve_source_bytes(format: ImageFormat) -> bool {
    // Public API docs explicitly call out non-animated GIF support only.
    // Preserve byte-for-byte only for formats we can safely pass through.
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    )
}

fn encode_image(
    image: &DynamicImage,
    preferred_format: ImageFormat,
    metadata: ImageMetadata,
) -> Result<(Vec<u8>, ImageFormat), ImageProcessingError> {
    let format = match preferred_format {
        ImageFormat::Jpeg | ImageFormat::WebP => preferred_format,
        _ => ImageFormat::Png,
    };
    let image = eight_bit_image(image);
    // Retain source PNG color interpretation without changing sample encoding.
    // Copy complete, bounded chunks (including their original CRCs) only into PNG.
    let color_chunks = metadata.png_color_chunks.clone();
    let mut buffer = Vec::new();
    let encoded = match format {
        ImageFormat::Jpeg => write_with_metadata(
            JpegEncoder::new_with_quality(&mut buffer, 85),
            &image,
            metadata,
        ),
        ImageFormat::WebP => {
            write_with_metadata(WebPEncoder::new_lossless(&mut buffer), &image, metadata)
        }
        _ => write_with_metadata(PngEncoder::new(&mut buffer), &image, metadata),
    };
    encoded.map_err(|source| ImageProcessingError::Encode { format, source })?;
    if format == ImageFormat::Png && !color_chunks.is_empty() {
        buffer.splice(33..33, color_chunks);
    }

    Ok((buffer, format))
}

/// Narrows wider samples to 8 bits but keeps the decoded channel layout, so
/// opaque or grayscale images are not expanded to RGBA before encoding.
fn eight_bit_image(image: &DynamicImage) -> Cow<'_, DynamicImage> {
    let color = image.color();
    if color.bytes_per_pixel() == color.channel_count() {
        return Cow::Borrowed(image);
    }
    Cow::Owned(match (color.has_color(), color.has_alpha()) {
        (false, false) => image.to_luma8().into(),
        (false, true) => image.to_luma_alpha8().into(),
        (true, false) => image.to_rgb8().into(),
        (true, true) => image.to_rgba8().into(),
    })
}

fn write_with_metadata(
    mut encoder: impl ImageEncoder,
    image: &DynamicImage,
    ImageMetadata {
        icc_profile, exif, ..
    }: ImageMetadata,
) -> image::ImageResult<()> {
    if let Some(icc_profile) = icc_profile {
        encoder
            .set_icc_profile(icc_profile)
            .map_err(image::ImageError::Unsupported)?;
    }
    if let Some(exif) = exif {
        encoder
            .set_exif_metadata(exif)
            .map_err(image::ImageError::Unsupported)?;
    }
    image.write_with_encoder(encoder)
}

/// Traverse length-delimited PNG chunks without decoding pixels or allocating
/// ancillary payloads. Invalid/truncated containers stop inspection safely.
fn png_chunks(bytes: &[u8]) -> impl Iterator<Item = (&[u8], &[u8], &[u8])> {
    let mut rest = bytes.get(8..).unwrap_or_default();
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let chunk = rest.get(..size.checked_add(12)?)?;
        let kind = &chunk[4..8];
        rest = &rest[chunk.len()..];
        Some((kind, &chunk[8..8 + size], chunk))
    })
}

// The png decoder bounds ICC expansion but silently drops an oversized profile.
// Reject that source explicitly, including byte-preserving and dimension-only
// paths, without allocating its decompressed profile a second time.
fn check_png_icc_budget(bytes: &[u8]) -> image::ImageResult<()> {
    use std::io::Read;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(());
    }
    let mut total = 0;
    for (kind, data, _) in png_chunks(bytes) {
        if kind != b"iCCP" {
            continue;
        }
        let Some(name_end) = data.iter().position(|byte| *byte == 0) else {
            continue;
        };
        if !(1..=79).contains(&name_end) || data.get(name_end + 1) != Some(&0) {
            continue;
        }
        let remaining = MAX_IMAGE_METADATA_BYTES - total;
        let mut decoder =
            flate2::bufread::ZlibDecoder::new(&data[name_end + 2..]).take(remaining + 1);
        let expanded = std::io::copy(&mut decoder, &mut std::io::sink())?;
        if expanded > remaining {
            return Err(image::ImageError::Limits(
                image::error::LimitError::from_kind(
                    image::error::LimitErrorKind::InsufficientMemory,
                ),
            ));
        }
        total += expanded;
    }
    Ok(())
}

fn png_color_chunks(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut seen = 0u8;
    for (kind, data, chunk) in png_chunks(bytes) {
        let (bit, size) = match kind {
            b"gAMA" => (1, 4),
            b"cHRM" => (2, 32),
            b"sRGB" => (4, 1),
            b"cICP" => (8, 4),
            _ => continue,
        };
        if seen & bit == 0 && data.len() == size {
            result.extend_from_slice(chunk);
            seen |= bit;
        }
    }
    result
}

fn image_has_animation(bytes: &[u8], format: ImageFormat) -> bool {
    match format {
        ImageFormat::Png => {
            png_chunks(bytes).any(|(kind, data, _)| kind == b"acTL" && data.len() == 8)
        }
        ImageFormat::WebP => {
            let mut rest = bytes.get(12..).unwrap_or_default();
            while let Some(header) = rest.first_chunk::<8>() {
                let [_, _, _, _, size_bytes @ ..] = *header;
                let size = u32::from_le_bytes(size_bytes) as usize;
                let Some(chunk) = rest.get(..8usize.saturating_add(size).saturating_add(size % 2))
                else {
                    break;
                };
                if &header[..4] == b"ANIM"
                    || (&header[..4] == b"VP8X" && chunk.get(8).is_some_and(|flags| flags & 2 != 0))
                {
                    return true;
                }
                rest = &rest[chunk.len()..];
            }
            false
        }
        ImageFormat::Gif => gif_has_multiple_frames(bytes),
        _ => false,
    }
}

fn gif_has_multiple_frames(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..13) else {
        return false;
    };
    let color_table = |flags: u8| {
        if flags & 0x80 != 0 {
            3usize << ((flags & 7) + 1)
        } else {
            0
        }
    };
    let mut offset = 13 + color_table(header[10]);
    let mut frames = 0;
    while let Some(marker) = bytes.get(offset) {
        offset += 1;
        match marker {
            0x2c => {
                frames += 1;
                if frames > 1 {
                    return true;
                }
                let Some(descriptor) = bytes.get(offset..offset + 9) else {
                    return false;
                };
                offset += 9 + color_table(descriptor[8]) + 1; // LZW minimum code size
            }
            0x21 => offset += 1, // extension label, followed by sub-blocks
            _ => return false,
        }
        loop {
            let Some(&length) = bytes.get(offset) else {
                return false;
            };
            offset += 1;
            if length == 0 {
                break;
            }
            offset += usize::from(length);
        }
    }
    false
}

fn format_to_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
