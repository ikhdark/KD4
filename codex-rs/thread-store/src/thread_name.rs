/// Trims a persisted thread name and rejects names that contain only whitespace.
pub fn normalize_thread_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_rejects_empty_names() {
        assert_eq!(normalize_thread_name("   "), None);
        assert_eq!(
            normalize_thread_name("  my thread  "),
            Some("my thread".to_string())
        );
    }
}
