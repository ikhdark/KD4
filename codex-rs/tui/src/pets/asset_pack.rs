//! Built-in pet asset acquisition and cache ownership.
//!
//! Unlike custom pets, built-in pets are not checked into the TUI package as
//! local spritesheets. The TUI resolves them from the public Codex pets CDN on
//! first use, verifies that the downloaded file has the expected spritesheet
//! geometry, and installs it into a versioned cache under CODEX_HOME.
//!
//! This module deliberately stops at "a validated spritesheet exists at this
//! path". Higher layers remain responsible for deciding when downloads are
//! allowed, when previews should block on them, and when a successfully loaded
//! built-in pet is safe to persist to config.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_http_client::BlockingHttpClientBuilder;
use url::Url;

use super::catalog;

const PET_PACK_VERSION: &str = "v1";
const PET_PACK_DIR: &str = "cache/tui-pets";
const PET_CDN_BASE_URL: &str = "https://persistent.oaistatic.com/codex/pets/v1";
const PET_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const PET_MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn builtin_spritesheet_path(codex_home: &Path, file: &str) -> PathBuf {
    pack_dir(codex_home).join("assets").join(file)
}

/// Ensure that a built-in pet's spritesheet is present and structurally valid.
///
/// The cache key is the CDN-facing filename, so updating a built-in pet means
/// publishing a new versioned filename rather than mutating an existing one in
/// place. If a cached file is missing or invalid, this downloads a fresh copy,
/// validates the decoded image dimensions, and installs it atomically. Callers
/// should treat any error here as "the asset is unavailable", not as a partial
/// install they can safely ignore.
pub(crate) fn ensure_builtin_pet(codex_home: &Path, pet: catalog::BuiltinPet) -> Result<()> {
    let destination = builtin_spritesheet_path(codex_home, pet.spritesheet_file);
    if validate_builtin_cache(codex_home, pet, &destination).is_ok() {
        return Ok(());
    }

    let url = builtin_pet_url(pet)?;
    let bytes = download_bytes_with_limit(&url, PET_MAX_DOWNLOAD_BYTES)?;
    let parent = destination
        .parent()
        .context("pet spritesheet path should include an assets directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    install_downloaded_spritesheet(&bytes, &destination)
}

fn builtin_pet_url(pet: catalog::BuiltinPet) -> Result<String> {
    let url = format!("{PET_CDN_BASE_URL}/{}", pet.spritesheet_file);
    validate_download_url(&url)?;
    Ok(url)
}

fn pack_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(PET_PACK_DIR).join(PET_PACK_VERSION)
}

fn download_bytes_with_limit(url: &str, max_bytes: u64) -> Result<Vec<u8>> {
    validate_download_url(url)?;
    let client = BlockingHttpClientBuilder::new()
        .request_timeout(Some(PET_DOWNLOAD_TIMEOUT))
        .build_with_transport_default_proxy()
        .context("build pet asset download client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("download pet asset from {url}"))?
        .error_for_status()
        .with_context(|| format!("download pet asset from {url}"))?;
    validate_download_url(response.url())?;

    if response.content_length().is_some_and(|len| len > max_bytes) {
        bail!("pet asset download from {url} exceeded {max_bytes} bytes");
    }

    let mut bytes = Vec::new();
    response
        .take(max_bytes.saturating_add(/*rhs*/ 1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read pet asset download from {url}"))?;
    if bytes.len() as u64 > max_bytes {
        bail!("pet asset download from {url} exceeded {max_bytes} bytes");
    }
    Ok(bytes)
}

fn install_downloaded_spritesheet(bytes: &[u8], destination: &Path) -> Result<()> {
    let dimensions = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?;
    if dimensions != (catalog::SPRITESHEET_WIDTH, catalog::SPRITESHEET_HEIGHT) {
        bail!("invalid downloaded pet spritesheet dimensions");
    }
    image::load_from_memory(bytes).context("decode downloaded pet spritesheet")?;
    let staging =
        tempfile::NamedTempFile::new_in(destination.parent().context("missing assets directory")?)?;
    fs::write(staging.path(), bytes).context("write downloaded pet spritesheet")?;
    staging
        .persist(destination)
        .with_context(|| format!("install {}", destination.display()))?;
    Ok(())
}

fn validate_builtin_cache(
    codex_home: &Path,
    builtin: catalog::BuiltinPet,
    path: &Path,
) -> Result<()> {
    validate_cached_spritesheet(path)?;
    let pet = super::model::Pet::load_with_codex_home(builtin.id, Some(codex_home))?;
    let bytes = pet.spritesheet_bytes()?;
    let dimensions = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()?
        .into_dimensions()?;
    if dimensions != (catalog::SPRITESHEET_WIDTH, catalog::SPRITESHEET_HEIGHT) {
        bail!("invalid cached pet spritesheet dimensions");
    }
    // A complete cache in this content namespace proves that this exact source
    // has already decoded successfully. Cold or corrupt sources must decode.
    let frames = pet.frame_cache_dir(codex_home, &bytes).join("frames");
    if super::frames::cached_png_frames(&pet, &frames).is_none() {
        image::load_from_memory(&bytes).context("decode cached pet spritesheet")?;
    }
    Ok(())
}

fn validate_download_url(value: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("parse pet asset download URL {value}"))?;
    if url.scheme() != "https" {
        bail!("unsupported pet asset download URL scheme {}", url.scheme());
    }
    Ok(())
}

fn validate_cached_spritesheet(path: &Path) -> Result<()> {
    let (width, height) =
        image::image_dimensions(path).with_context(|| format!("read {}", path.display()))?;
    if width != catalog::SPRITESHEET_WIDTH || height != catalog::SPRITESHEET_HEIGHT {
        bail!(
            "invalid pet spritesheet dimensions for {}: expected {}x{}, got {}x{}",
            path.display(),
            catalog::SPRITESHEET_WIDTH,
            catalog::SPRITESHEET_HEIGHT,
            width,
            height
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_test_pack(codex_home: &Path) {
    let assets_dir = pack_dir(codex_home).join("assets");
    fs::create_dir_all(&assets_dir).unwrap();
    for pet in catalog::BUILTIN_PETS {
        let path = assets_dir.join(pet.spritesheet_file);
        catalog::write_test_spritesheet(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn downloaded_spritesheet_replacement_is_validated_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("pet.webp");
        catalog::write_test_spritesheet(&destination);
        let original = fs::read(&destination).unwrap();
        assert!(install_downloaded_spritesheet(b"corrupt image", &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), original);

        let replacement = image::RgbaImage::from_pixel(
            catalog::SPRITESHEET_WIDTH,
            catalog::SPRITESHEET_HEIGHT,
            image::Rgba([10, 20, 30, 255]),
        );
        let mut bytes = std::io::Cursor::new(Vec::new());
        replacement
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .unwrap();
        install_downloaded_spritesheet(bytes.get_ref(), &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), *bytes.get_ref());
        assert_eq!(image::open(&destination).unwrap().to_rgba8(), replacement);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn builtin_pet_url_uses_public_cdn_path() {
        let pet = catalog::builtin_pet("dewey").unwrap();

        let url = builtin_pet_url(pet).unwrap();

        assert_eq!(
            url,
            "https://persistent.oaistatic.com/codex/pets/v1/dewey-spritesheet-v4.webp"
        );
    }

    #[test]
    fn write_test_pack_installs_all_builtins() {
        let dir = tempfile::tempdir().unwrap();

        write_test_pack(dir.path());

        for pet in catalog::BUILTIN_PETS {
            let path = builtin_spritesheet_path(dir.path(), pet.spritesheet_file);
            assert!(path.is_file());
            validate_cached_spritesheet(&path).unwrap();
        }
    }
}
