/// Trims a persisted thread name and rejects names that contain only whitespace.
pub fn normalize_thread_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

