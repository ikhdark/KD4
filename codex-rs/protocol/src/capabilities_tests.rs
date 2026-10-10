use super::CapabilityRootLocation;
use super::SelectedCapabilityRoot;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;

#[test]
fn environment_capability_root_accepts_native_absolute_path() {
    let native = std::env::current_dir().unwrap();
    let root: SelectedCapabilityRoot = serde_json::from_value(serde_json::json!({
        "id": "native",
        "location": {"type": "environment", "environmentId": "executor", "path": native}
    }))
    .unwrap();
    assert_eq!(
        root,
        SelectedCapabilityRoot {
            id: "native".to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: "executor".to_string(),
                path: PathUri::from(
                    codex_utils_absolute_path::AbsolutePathBuf::try_from(native).unwrap()
                ),
            },
        }
    );
}

#[test]
fn environment_capability_root_rejects_relative_path() {
    let error = serde_json::from_value::<SelectedCapabilityRoot>(serde_json::json!({
        "id": "relative",
        "location": {"type": "environment", "environmentId": "executor", "path": "plugins/demo"}
    }))
    .expect_err("relative path must be rejected");
    assert_eq!(error.to_string(), "path `plugins/demo` is not absolute");
}

#[test]
fn environment_capability_root_accepts_foreign_file_uri() {
    // The drive URI is native on Windows; the POSIX URI is the foreign convention here.
    for uri in ["file:///C:/plugins/demo", "file:///opt/plugins/demo"] {
        let selected_root: SelectedCapabilityRoot = serde_json::from_value(serde_json::json!({
            "id": "selected-demo",
            "location": {"type": "environment", "environmentId": "executor-test", "path": uri}
        }))
        .expect("file URI should deserialize");

        assert_eq!(
            selected_root,
            SelectedCapabilityRoot {
                id: "selected-demo".to_string(),
                location: CapabilityRootLocation::Environment {
                    environment_id: "executor-test".to_string(),
                    path: PathUri::parse(uri).expect("path URI"),
                },
            }
        );
        let CapabilityRootLocation::Environment { path, .. } = selected_root.location;
        assert_eq!(path.as_str(), uri);
    }
}
