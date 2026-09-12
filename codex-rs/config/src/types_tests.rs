use super::*;
use pretty_assertions::assert_eq;

#[test]
fn otel_value_types_preserve_wire_names_and_optional_tls_paths() {
    let protocol: codex_protocol::config_types::OtelHttpProtocol =
        serde_json::from_str("\"binary\"").expect("binary protocol should deserialize");
    assert_eq!(protocol, OtelHttpProtocol::Binary);
    assert_eq!(
        serde_json::to_string(&OtelHttpProtocol::Json).unwrap(),
        "\"json\""
    );
    assert!(serde_json::from_str::<OtelHttpProtocol>("\"unknown\"").is_err());

    let home = tempfile::tempdir().expect("temporary directory");
    let ca_path = home.path().join("ca.pem");
    let tls: codex_protocol::config_types::OtelTlsConfig =
        serde_json::from_value(serde_json::json!({"ca-certificate": ca_path}))
            .expect("TLS paths should deserialize");
    assert_eq!(tls.ca_certificate.as_ref().unwrap().as_path(), ca_path);
    assert_eq!(tls.client_certificate, None);
    assert_eq!(tls.client_private_key, None);
    assert_eq!(
        serde_json::to_value(&tls).unwrap(),
        serde_json::json!({
            "ca-certificate": ca_path,
            "client-certificate": null,
            "client-private-key": null,
        })
    );
}

#[test]
fn deserialize_skill_config_with_name_selector() {
    let cfg: SkillConfig = toml::from_str(
        r#"
            name = "github:yeet"
            enabled = false
        "#,
    )
    .expect("should deserialize skill config with name selector");

    assert_eq!(cfg.name.as_deref(), Some("github:yeet"));
    assert_eq!(cfg.path, None);
    assert!(!cfg.enabled);
}

#[test]
fn deserialize_skill_config_with_path_selector() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let skill_path = tempdir.path().join("skills").join("demo").join("SKILL.md");
    let cfg: SkillConfig = toml::from_str(&format!(
        r#"
            path = {path:?}
            enabled = false
        "#,
        path = skill_path.display().to_string(),
    ))
    .expect("should deserialize skill config with path selector");

    assert_eq!(
        cfg,
        SkillConfig {
            path: Some(
                AbsolutePathBuf::from_absolute_path(&skill_path)
                    .expect("skill path should be absolute"),
            ),
            name: None,
            enabled: false,
        }
    );
}

#[test]
fn memories_config_clamps_count_limits_to_nonzero_values() {
    let config = MemoriesConfig::from(MemoriesToml {
        max_raw_memories_for_consolidation: Some(0),
        max_rollouts_per_startup: Some(0),
        ..Default::default()
    });

    assert_eq!(
        config,
        MemoriesConfig {
            max_raw_memories_for_consolidation: 1,
            max_rollouts_per_startup: 1,
            ..MemoriesConfig::default()
        }
    );
}

#[test]
fn memories_config_clamps_rate_limit_remaining_threshold() {
    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(101),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 100,
            ..MemoriesConfig::default()
        }
    );

    let config = MemoriesConfig::from(MemoriesToml {
        min_rate_limit_remaining_percent: Some(-1),
        ..Default::default()
    });
    assert_eq!(
        config,
        MemoriesConfig {
            min_rate_limit_remaining_percent: 0,
            ..MemoriesConfig::default()
        }
    );
}
