use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use image::GenericImageView;

use super::model::Pet;

pub(super) fn cached_png_frames(pet: &Pet, frame_dir: &Path) -> Option<Vec<PathBuf>> {
    let paths = frame_paths(pet, frame_dir);
    paths.iter().all(|path| path.is_file()).then_some(paths)
}

fn frame_paths(pet: &Pet, frame_dir: &Path) -> Vec<PathBuf> {
    (0..pet.frame_count())
        .map(|index| frame_dir.join(format!("frame_{index:03}.png")))
        .collect()
}

pub(super) fn prepare_png_frames(
    pet: &Pet,
    bytes: &[u8],
    frame_dir: &Path,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(frame_dir).with_context(|| format!("create {}", frame_dir.display()))?;

    let expected = frame_paths(pet, frame_dir);

    let complete = expected.iter().all(|path| path.is_file());
    if !complete {
        let dimensions = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()?
            .into_dimensions()?;
        anyhow::ensure!(
            dimensions == (pet.frame_width * pet.columns, pet.frame_height * pet.rows),
            "spritesheet dimensions changed after loading pet"
        );
        let spritesheet = image::load_from_memory(bytes)
            .with_context(|| format!("read {}", pet.spritesheet_path.display()))?;
        for row in 0..pet.rows {
            for column in 0..pet.columns {
                let index = row
                    .checked_mul(pet.columns)
                    .and_then(|row_offset| row_offset.checked_add(column))
                    .context("pet frame index overflow")?;
                let index = usize::try_from(index).context("pet frame index does not fit usize")?;
                let path = expected
                    .get(index)
                    .context("pet frame index exceeds expected frame count")?;
                if path.is_file() {
                    continue;
                }
                let x = column
                    .checked_mul(pet.frame_width)
                    .context("pet frame x offset overflow")?;
                let y = row
                    .checked_mul(pet.frame_height)
                    .context("pet frame y offset overflow")?;
                let frame = spritesheet.try_view(x, y, pet.frame_width, pet.frame_height)?;
                let staging = tempfile::NamedTempFile::new_in(frame_dir)?;
                frame
                    .to_image()
                    .save_with_format(staging.path(), image::ImageFormat::Png)
                    .with_context(|| format!("write {}", path.display()))?;
                staging
                    .persist(path)
                    .with_context(|| format!("publish {}", path.display()))?;
            }
        }
    }

    Ok(expected)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use image::ImageBuffer;
    use image::Rgba;

    use super::*;

    #[test]
    fn prepare_png_frames_slices_spritesheet_without_external_command() {
        let dir = tempfile::tempdir().unwrap();
        let spritesheet_path = dir.path().join("spritesheet.png");
        let spritesheet: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 255, 0, 255])
            }
        });
        spritesheet.save(&spritesheet_path).unwrap();

        let bytes = fs::read(&spritesheet_path).unwrap();
        let pet = Pet {
            display_name: "Tiny".to_string(),
            description: String::new(),
            spritesheet_path: spritesheet_path.clone(),
            frame_width: 1,
            frame_height: 1,
            columns: 2,
            rows: 1,
            frame_count: 2,
            animations: HashMap::new(),
        };
        let frame_dir = dir.path().join("frames");
        // Extraction must use the same bytes as the cache key even if the source changes.
        fs::write(&spritesheet_path, b"replaced source").unwrap();
        let frames = prepare_png_frames(&pet, &bytes, &frame_dir).unwrap();

        assert_eq!(frames.len(), 2);
        let first = image::open(&frames[0]).unwrap().to_rgba8();
        let second = image::open(&frames[1]).unwrap().to_rgba8();
        assert_eq!(first.dimensions(), (1, 1));
        assert_eq!(second.dimensions(), (1, 1));
        assert_eq!(first.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
        assert_eq!(second.get_pixel(0, 0), &Rgba([0, 255, 0, 255]));

        let first_modified = fs::metadata(&frames[0]).unwrap().modified().unwrap();
        fs::remove_file(&frames[1]).unwrap();
        assert_eq!(
            prepare_png_frames(&pet, &bytes, &frame_dir).unwrap(),
            frames
        );
        assert_eq!(
            fs::metadata(&frames[0]).unwrap().modified().unwrap(),
            first_modified
        );
        assert_eq!(image::open(&frames[1]).unwrap().to_rgba8(), second);
        assert_eq!(fs::read_dir(&frame_dir).unwrap().count(), 2);
    }
}
