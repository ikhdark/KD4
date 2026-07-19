use std::path::Component;
use std::path::Path;

/// Returns a stable repository route for a repository-relative source path.
///
/// Rust crates are routed by their directory immediately below `codex-rs`.
/// The remaining repository-owned surfaces use their top-level directory.
pub fn source_map_route_for_path(path: &Path) -> Option<String> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(component) => component.to_str().map(str::to_ascii_lowercase),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;

    let top_level = components.first()?;
    match top_level.as_str() {
        "codex-rs" if components.len() <= 2 => Some("codex-rs".to_string()),
        "codex-rs" => components.get(1).cloned(),
        "codex-cli" | "docs" | "scripts" | "third_party" if components.len() >= 2 => {
            Some(top_level.replace('_', "-"))
        }
        "sdk" if components.len() >= 3 => components.get(1).map(|sdk| format!("sdk-{sdk}")),
        _ => None,
    }
}

#[cfg(test)]
#[path = "source_routes_tests.rs"]
mod tests;
