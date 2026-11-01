pub(crate) const REMOTE_IMAGE_URL_ERROR: &str =
    "remote image URLs are not supported; use an inline data URL instead";

pub(crate) fn image_url_error(image_url: &str) -> Option<&'static str> {
    use codex_protocol::models::ImageUrlKind;
    use codex_protocol::models::classify_image_url;
    match classify_image_url(image_url) {
        ImageUrlKind::InlineData => None,
        ImageUrlKind::Remote => Some(REMOTE_IMAGE_URL_ERROR),
        ImageUrlKind::Unsupported => {
            Some("unsupported image representation; use an inline base64 data URL instead")
        }
    }
}

