use super::PluginEventIdentity;
use super::read_events_for_remote_plugin;
use super::validate_mutation_events;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::SystemTime;

const REMOTE_PLUGIN_ID: &str = "plugins~Plugin_test";

#[test]
fn rejects_wrong_metadata_types() {
    for (field, value) in [
        ("has_skills", json!("true")),
        ("mcp_server_count", json!(-1)),
        ("mcp_server_count", json!(0.5)),
        ("connector_ids", json!([1])),
        ("product_client_id", json!(false)),
    ] {
        let mut installed = mutation_event("codex_plugin_installed");
        installed["event_params"][field] = value;
        let error = validate_mutation_events(vec![installed], expected_identity()).unwrap_err();
        assert!(error.to_string().contains(field), "{error}");
    }
}

#[test]
fn tolerates_only_incomplete_final_capture_records() {
    let path = unique_capture_path("partial");
    let installed = mutation_event("codex_plugin_installed");
    let complete = json!({"events": [installed]}).to_string();
    for tail in ["{", "{\"events\":[{\"name\":\"part"] {
        fs::write(&path, format!("{complete}\n{tail}")).unwrap();
        assert_eq!(
            read_events_for_remote_plugin(&path, REMOTE_PLUGIN_ID).unwrap(),
            vec![installed.clone()]
        );
    }
    for tail in ["{\n", "{oops", "{\"events\":false}\n"] {
        fs::write(&path, format!("{complete}\n{tail}")).unwrap();
        assert!(
            read_events_for_remote_plugin(&path, REMOTE_PLUGIN_ID).is_err(),
            "{tail}"
        );
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn reads_and_validates_remote_plugin_mutation_events() {
    let path = unique_capture_path("valid");
    let installed = mutation_event("codex_plugin_installed");
    let uninstalled = mutation_event("codex_plugin_uninstalled");
    let unrelated = json!({
        "event_type": "codex_plugin_installed",
        "event_params": {
            "plugin_id": "other@openai-curated-remote",
            "remote_plugin_id": "plugins~Plugin_other"
        }
    });
    let contents = [
        json!({"events": [unrelated]}),
        json!({"events": [installed, uninstalled]}),
    ]
    .into_iter()
    .map(|payload| serde_json::to_string(&payload).expect("serialize capture payload"))
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&path, contents).expect("write capture file");

    let events = read_events_for_remote_plugin(&path, REMOTE_PLUGIN_ID)
        .expect("read matching plugin events");
    let validated =
        validate_mutation_events(events, expected_identity()).expect("validate mutation events");

    assert_eq!(validated, vec![installed, uninstalled]);
    fs::remove_file(path).expect("remove capture file");
}

#[test]
fn rejects_duplicate_mutation_events() {
    let installed = mutation_event("codex_plugin_installed");
    let error = validate_mutation_events(vec![installed.clone(), installed], expected_identity())
        .expect_err("duplicate install events should fail validation");

    assert!(error.to_string().contains("found 2"));
}

#[test]
fn rejects_missing_capability_metadata() {
    let mut installed = mutation_event("codex_plugin_installed");
    installed["event_params"]["has_skills"] = Value::Null;
    let error = validate_mutation_events(vec![installed], expected_identity())
        .expect_err("missing capability metadata should fail validation");

    assert!(error.to_string().contains("has_skills"));
}

fn mutation_event(event_type: &str) -> Value {
    json!({
        "event_type": event_type,
        "event_params": {
            "plugin_id": "sample@openai-curated-remote",
            "remote_plugin_id": REMOTE_PLUGIN_ID,
            "plugin_name": "sample",
            "marketplace_name": "openai-curated-remote",
            "has_skills": true,
            "mcp_server_count": 0,
            "connector_ids": [],
            "product_client_id": "test-client"
        }
    })
}

fn expected_identity() -> PluginEventIdentity<'static> {
    PluginEventIdentity {
        plugin_id: "sample@openai-curated-remote",
        remote_plugin_id: REMOTE_PLUGIN_ID,
        plugin_name: "sample",
        marketplace_name: "openai-curated-remote",
    }
}

fn unique_capture_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "codex-plugin-analytics-capture-{name}-{}-{nonce}.jsonl",
        process::id()
    ))
}
