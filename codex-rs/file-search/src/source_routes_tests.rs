use super::source_map_route_for_path;
use pretty_assertions::assert_eq;
use std::path::Path;

#[test]
fn routes_rust_crates_by_crate_directory() {
    assert_eq!(
        source_map_route_for_path(Path::new("repo/codex-rs/app-server-protocol/src/lib.rs")),
        Some("app-server-protocol".to_string())
    );
}

#[test]
fn routes_top_level_and_sdk_surfaces() {
    assert_eq!(
        source_map_route_for_path(Path::new("repo/scripts/check.ps1")),
        Some("scripts".to_string())
    );
    assert_eq!(
        source_map_route_for_path(Path::new("repo/sdk/python/src/client.py")),
        Some("sdk-python".to_string())
    );
}
