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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_embedded_image_representations() {
        for url in ["data:image/png;base64,YQ==", "DATA:image/png;BASE64,YQ=="] {
            assert_eq!(image_url_error(url), None);
        }
        for url in [
            "file:///a.png",
            "ftp://a/a.png",
            "blob:a",
            "",
            "plain",
            " https://a/a.png",
            "HtTpS://a/a.png",
        ] {
            assert!(image_url_error(url).is_some());
        }
        assert_eq!(
            image_url_error("HTTPS://a/a.png"),
            Some(REMOTE_IMAGE_URL_ERROR)
        );
    }
}
