use std::path::Path;

/// Returns a stable repository route for a source path.
///
/// Rust crates are routed by their directory immediately below `codex-rs`.
/// The remaining repository-owned surfaces use their top-level directory.
pub fn source_map_route_for_path(path: &Path) -> Option<String> {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    if let Some(index) = components
        .iter()
        .position(|component| component == "codex-rs")
    {
        return components.get(index + 1).cloned();
    }

    for top_level in ["codex-cli", "docs", "scripts", "third_party"] {
        if components.iter().any(|component| component == top_level) {
            return Some(top_level.replace('_', "-"));
        }
    }

    let sdk_index = components.iter().position(|component| component == "sdk")?;
    let sdk = components.get(sdk_index + 1)?;
    Some(format!("sdk-{sdk}"))
}

#[cfg(test)]
#[path = "source_routes_tests.rs"]
mod tests;
