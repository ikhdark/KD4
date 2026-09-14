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
    let result = serde_json::from_value::<SelectedCapabilityRoot>(serde_json::json!({
        "id": "relative",
        "location": {"type": "environment", "environmentId": "executor", "path": "plugins/demo"}
    }));
    assert!(result.is_err());
}

#[test]
fn environment_capability_root_accepts_foreign_file_uri() {
    let selected_root = serde_json::from_str::<SelectedCapabilityRoot>(
        r#"{
            "id": "selected-demo",
            "location": {
                "type": "environment",
                "environmentId": "executor-test",
                "path": "file:///C:/plugins/demo"
            }
        }"#,
    )
    .expect("file URI should deserialize");

    assert_eq!(
        selected_root,
        SelectedCapabilityRoot {
            id: "selected-demo".to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: "executor-test".to_string(),
                path: PathUri::parse("file:///C:/plugins/demo").expect("path URI"),
            },
        }
    );
}
