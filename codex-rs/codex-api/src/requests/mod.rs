pub(crate) mod headers;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Zstd,
}

#[cfg(test)]
mod tests {
    #[test]
    fn compression_is_owned_by_requests_module() {
        let source = include_str!("mod.rs");

        assert!(source.contains(&["pub enum ", "Compression"].concat()));
        assert!(!source.contains(&["mod ", "responses;"].concat()));
    }
}
