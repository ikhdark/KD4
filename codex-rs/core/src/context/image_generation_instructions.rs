use std::fmt::Display;

/// Maximum size of the extension's model-facing generated-image path hint.
const MAX_IMAGE_GENERATION_OUTPUT_HINT_BYTES: usize = 1024;
const IMAGE_PRESERVATION_HINT: &str = "If you need to use a generated image at another path, copy it and leave the original in place unless the user explicitly asks you to delete it.";

/// Omits oversized path details while retaining the image-preservation instruction.
pub fn extension_image_generation_output_hint(
    image_output_dir: impl Display,
    image_output_path: impl Display,
) -> Option<String> {
    let hint = image_generation_hint(image_output_dir, image_output_path);
    Some(if hint.len() <= MAX_IMAGE_GENERATION_OUTPUT_HINT_BYTES {
        hint
    } else {
        IMAGE_PRESERVATION_HINT.to_string()
    })
}

fn image_generation_hint(
    image_output_dir: impl Display,
    image_output_path: impl Display,
) -> String {
    format!(
        "Generated images are saved to {image_output_dir} as {image_output_path} by default.\n{IMAGE_PRESERVATION_HINT}"
    )
}

#[cfg(test)]
mod tests {
    use super::extension_image_generation_output_hint;

    #[test]
    fn output_hint_preserves_paths_through_the_byte_limit() {
        let preservation = "If you need to use a generated image at another path, copy it and leave the original in place unless the user explicitly asks you to delete it.";
        assert_eq!(
            extension_image_generation_output_hint("/tmp", "/tmp/image.png"),
            Some(format!("Generated images are saved to /tmp as /tmp/image.png by default.\n{preservation}"))
        );
        let prefix = "Generated images are saved to /tmp as ";
        let suffix = format!(" by default.\n{preservation}");
        for start in ["", "🦀"] {
            let path = format!("{start}{}", "x".repeat(1024 - prefix.len() - suffix.len() - start.len()));
            let expected = format!("{prefix}{path}{suffix}");
            assert_eq!(expected.len(), 1024);
            assert_eq!(extension_image_generation_output_hint("/tmp", &path), Some(expected));
            assert_eq!(
                extension_image_generation_output_hint("/tmp", format!("{path}x")),
                Some(preservation.to_string())
            );
        }
    }
}
