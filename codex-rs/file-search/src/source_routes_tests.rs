use super::source_map_route_for_path;
use pretty_assertions::assert_eq;
use std::path::Path;

#[test]
fn routes_rust_crates_by_crate_directory() {
    assert_eq!(
        source_map_route_for_path(Path::new("codex-rs/app-server-protocol/src/lib.rs")),
        Some("app-server-protocol".to_string())
    );
    assert_eq!(
        source_map_route_for_path(Path::new("codex-rs/Cargo.toml")),
        Some("codex-rs".to_string())
    );
}

#[test]
fn routes_top_level_and_sdk_surfaces() {
    assert_eq!(
        source_map_route_for_path(Path::new("scripts/check.ps1")),
        Some("scripts".to_string())
    );
    assert_eq!(
        source_map_route_for_path(Path::new("sdk/python/src/client.py")),
        Some("sdk-python".to_string())
    );
    assert_eq!(
        source_map_route_for_path(Path::new("examples/docs/readme.md")),
        None
    );
    assert_eq!(
        source_map_route_for_path(Path::new("tools/codex-rs/fake/src/lib.rs")),
        None
    );
    assert_eq!(
        source_map_route_for_path(Path::new("examples/sdk/python/client.py")),
        None
    );
    assert_eq!(source_map_route_for_path(Path::new("sdk/python")), None);
    assert_eq!(source_map_route_for_path(Path::new("docs")), None);
}

#[test]
fn rejects_non_repository_relative_component_positions() {
    assert_eq!(
        source_map_route_for_path(Path::new("../codex-rs/core/src/lib.rs")),
        None
    );
    assert_eq!(
        source_map_route_for_path(Path::new("/repo/codex-rs/core/src/lib.rs")),
        None
    );
    assert_eq!(
        source_map_route_for_path(Path::new("codex-rs/core/../scripts/check.rs")),
        None
    );
}
