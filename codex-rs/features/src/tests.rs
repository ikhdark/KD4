use crate::Feature;
use crate::FeatureConfigSource;
use crate::FeatureConsumer;
use crate::FeatureOverrides;
use crate::FeatureToml;
use crate::Features;
use crate::FeaturesToml;
use crate::Stage;
use crate::feature_for_key;
use crate::feature_requirement_for_key;
use crate::is_known_feature_key;
use crate::unstable_features_warning_event;
use crate::user_settable_feature_for_key;
use crate::user_settable_features;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use toml::Table;
use toml::Value as TomlValue;

#[test]
fn feature_metadata_is_exhaustive_and_single_sourced() {
    assert_eq!(crate::ALL_FEATURES.len(), crate::FEATURES.len());

    for (feature, spec) in crate::ALL_FEATURES.iter().zip(crate::FEATURES) {
        assert_eq!(*feature, spec.id);
        assert_eq!(feature.key(), spec.key);
        assert_eq!(feature.stage(), spec.stage);
        assert_eq!(feature.default_enabled(), spec.default_enabled);
        assert_eq!(feature.consumer(), spec.consumer);
    }
}

#[test]
fn machine_readable_registry_projection_uses_evaluated_defaults() {
    let entries = crate::feature_registry_entries();

    assert_eq!(entries.len(), crate::FEATURES.len());
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.key == Feature::SecretAuthStorage.key())
            .map(|entry| entry.default_enabled),
        Some(true)
    );
    assert_eq!(
        serde_json::to_value(&entries).expect("serialize feature registry"),
        serde_json::json!(
            crate::FEATURES
                .iter()
                .map(|spec| serde_json::json!({
                    "key": spec.key,
                    "defaultEnabled": spec.default_enabled,
                    "consumer": spec.consumer,
                }))
                .collect::<Vec<_>>()
        )
    );
}

#[test]
fn user_settable_registry_excludes_internal_features_and_legacy_aliases() {
    assert_eq!(
        user_settable_feature_for_key("unified_exec"),
        Some(Feature::UnifiedExec)
    );
    assert_eq!(
        user_settable_feature_for_key("terminal_resize_reflow"),
        None
    );
    assert_eq!(
        user_settable_feature_for_key("experimental_use_unified_exec_tool"),
        None
    );
    assert!(user_settable_features().all(|spec| !matches!(spec.stage, Stage::Internal)));
}

#[test]
fn managed_requirement_alias_resolves_only_through_requirement_lookup() {
    assert_eq!(
        feature_requirement_for_key("auto_review"),
        Some(Feature::GuardianApproval)
    );
    assert_eq!(user_settable_feature_for_key("auto_review"), None);
    assert_eq!(feature_for_key("auto_review"), None);
}

#[test]
fn deleted_zombie_feature_keys_are_unknown() {
    for key in ["artifact", "enable_mcp_apps", "realtime_conversation"] {
        assert_eq!(feature_for_key(key), None, "{key}");
        assert_eq!(user_settable_feature_for_key(key), None, "{key}");
    }
}

#[test]
fn under_development_features_are_disabled_by_default() {
    for spec in crate::FEATURES {
        if matches!(spec.stage, Stage::UnderDevelopment) {
            assert_eq!(
                spec.default_enabled, false,
                "feature `{}` is under development and must be disabled by default",
                spec.key
            );
        }
    }
}

#[test]
fn default_enabled_features_are_stable() {
    for spec in crate::FEATURES {
        if spec.default_enabled {
            assert!(
                matches!(spec.stage, Stage::Stable),
                "feature `{}` is enabled by default but is not stable ({:?})",
                spec.key,
                spec.stage
            );
        }
    }
}

#[test]
fn retired_feature_keys_are_unknown_and_ignored_by_feature_sources() {
    for key in [
        "terminal_resize_reflow",
        "apps_mcp_path_override",
        "item_ids",
        "remote_compaction_v2",
        "mentions_v2",
        "image_detail_original",
        "remote_control",
        "workspace_dependencies",
        "chronicle",
        "telepathy",
        "resize_all_images",
        "undo",
        "js_repl",
        "js_repl_tools_only",
        "apply_patch_freeform",
        "plugin_hooks",
        "tool_search_always_defer_mcp_tools",
    ] {
        assert!(!is_known_feature_key(key), "{key}");
        assert_eq!(feature_for_key(key), None, "{key}");

        let features_toml = FeaturesToml::from(BTreeMap::from([(key.to_string(), true)]));
        let features = Features::from_sources(
            FeatureConfigSource {
                features: Some(&features_toml),
            },
            FeatureConfigSource::default(),
            FeatureOverrides::default(),
        );

        assert_eq!(features, Features::with_defaults(), "{key}");
    }
}

#[test]
fn removed_code_mode_waiting_policy_is_rejected() {
    toml::from_str::<FeaturesToml>(
        r#"
[code_mode]
enabled = true
waiting_policy = "yield_after"
"#,
    )
    .expect_err("removed waiting_policy field should be rejected");
}

#[test]
fn strict_feature_config_still_rejects_unknown_fields() {
    let contents = r#"
[code_mode]
enabled = true
not_a_real_field = 1
"#;
    toml::from_str::<FeaturesToml>(contents)
        .expect_err("unknown structured feature fields should still be rejected");
}

#[test]
fn code_mode_only_requires_code_mode() {
    let mut features = Features::with_defaults();
    features.disable(Feature::CodeMode);
    features.enable(Feature::CodeModeOnly);
    features.normalize_dependencies();

    assert_eq!(features.enabled(Feature::CodeModeOnly), true);
    assert_eq!(features.enabled(Feature::CodeMode), true);
}

#[test]
fn code_mode_host_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::CodeModeHost.stage(), Stage::Stable);
    assert_eq!(Feature::CodeModeHost.default_enabled(), true);
    assert_eq!(
        feature_for_key("code_mode_host"),
        Some(Feature::CodeModeHost)
    );
}

#[test]
fn guardian_approval_is_stable_and_enabled_by_default() {
    let spec = Feature::GuardianApproval.info();

    assert_eq!(spec.stage, Stage::Stable);
    assert_eq!(Feature::GuardianApproval.default_enabled(), true);
}

#[test]
fn completed_runtime_mechanisms_are_stable_and_enabled_by_default() {
    for feature in [
        Feature::DeferredExecutor,
        Feature::CodeMode,
        Feature::LocalThreadStoreCompression,
        Feature::ApplyPatchStreamingEvents,
        Feature::ExecPermissionApprovals,
        Feature::RequestPermissionsTool,
        Feature::MultiAgentV2,
        Feature::TaskCompletionReviewer,
    ] {
        assert_eq!(feature.stage(), Stage::Stable, "{feature:?}");
        assert_eq!(feature.default_enabled(), true, "{feature:?}");
    }
}

#[test]
fn tool_suggest_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::ToolSuggest.stage(), Stage::Stable);
    assert_eq!(Feature::ToolSuggest.default_enabled(), true);
}

#[test]
fn network_proxy_is_experimental_and_disabled_by_default() {
    assert_eq!(
        feature_for_key("network_proxy"),
        Some(Feature::NetworkProxy)
    );
    assert!(matches!(
        Feature::NetworkProxy.stage(),
        Stage::Experimental { .. }
    ));
    assert_eq!(Feature::NetworkProxy.default_enabled(), false);
}

#[test]
fn secret_auth_storage_defaults_to_enabled() {
    assert_eq!(Feature::SecretAuthStorage.stage(), Stage::Stable);
    assert!(Feature::SecretAuthStorage.default_enabled());
    assert_eq!(
        feature_for_key("secret_auth_storage"),
        Some(Feature::SecretAuthStorage)
    );
}

#[test]
fn browser_controls_are_stable_and_enabled_by_default() {
    assert_eq!(Feature::InAppBrowser.stage(), Stage::Stable);
    assert_eq!(Feature::InAppBrowser.default_enabled(), true);
    assert_eq!(
        feature_for_key("in_app_browser"),
        Some(Feature::InAppBrowser)
    );

    assert_eq!(Feature::BrowserUse.stage(), Stage::Stable);
    assert_eq!(Feature::BrowserUse.default_enabled(), true);
    assert_eq!(feature_for_key("browser_use"), Some(Feature::BrowserUse));

    assert_eq!(Feature::BrowserUseExternal.stage(), Stage::Stable);
    assert_eq!(Feature::BrowserUseExternal.default_enabled(), true);
    assert_eq!(
        feature_for_key("browser_use_external"),
        Some(Feature::BrowserUseExternal)
    );

    assert_eq!(Feature::ComputerUse.stage(), Stage::Stable);
    assert_eq!(Feature::ComputerUse.default_enabled(), true);
    assert_eq!(feature_for_key("computer_use"), Some(Feature::ComputerUse));

    for feature in [
        Feature::InAppBrowser,
        Feature::BrowserUse,
        Feature::BrowserUseFullCdpAccess,
        Feature::BrowserUseExternal,
        Feature::ComputerUse,
    ] {
        assert_eq!(feature.consumer(), FeatureConsumer::Client);
    }
}

#[test]
fn client_only_features_are_not_read_by_rust_runtime_sources() {
    fn visit_rs_sources(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(path).expect("read Rust workspace directory") {
            let entry = entry.expect("read Rust workspace entry");
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                visit_rs_sources(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("features crate is in the Rust workspace");
    let mut files = Vec::new();
    visit_rs_sources(workspace, &mut files);
    let client_only_names = [
        "Feature::InAppBrowser",
        "Feature::BrowserUse",
        "Feature::BrowserUseFullCdpAccess",
        "Feature::BrowserUseExternal",
        "Feature::ComputerUse",
    ];
    let mut violations = Vec::new();

    for path in files {
        let relative = path
            .strip_prefix(workspace)
            .expect("workspace-relative path");
        if relative.starts_with("features")
            || relative
                .components()
                .any(|component| component.as_os_str() == "tests")
            || path.file_name().is_some_and(|name| name == "tests.rs")
            || path
                .file_stem()
                .is_some_and(|name| name.to_string_lossy().ends_with("_tests"))
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for name in client_only_names {
            if source.contains(name) {
                violations.push(format!("{} reads {name}", relative.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "client-only feature flags must not select Rust runtime behavior:\n{}",
        violations.join("\n")
    );
}

#[test]
fn image_generation_is_stable_and_legacy_alias_is_unknown() {
    assert_eq!(Feature::ImageGeneration.stage(), Stage::Stable);
    assert_eq!(Feature::ImageGeneration.default_enabled(), true);
    assert_eq!(
        feature_for_key("image_generation"),
        Some(Feature::ImageGeneration)
    );
    assert_eq!(feature_for_key("imagegenext"), None);
}

#[test]
fn image_generation_toggle_controls_extension_backed_generation() {
    let mut entries = BTreeMap::new();
    entries.insert("image_generation".to_string(), false);
    let mut features = Features::with_defaults();
    features.apply_map(&entries);
    assert!(!features.enabled(Feature::ImageGeneration));

    entries.insert("image_generation".to_string(), true);
    features.disable(Feature::ImageGeneration);
    features.apply_map(&entries);
    assert!(features.enabled(Feature::ImageGeneration));
}

#[test]
fn tool_call_mcp_elicitation_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::ToolCallMcpElicitation.stage(), Stage::Stable);
    assert_eq!(Feature::ToolCallMcpElicitation.default_enabled(), true);
}

#[test]
fn auth_elicitation_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::AuthElicitation.stage(), Stage::Stable);
    assert_eq!(Feature::AuthElicitation.default_enabled(), true);
    assert_eq!(
        feature_for_key("auth_elicitation"),
        Some(Feature::AuthElicitation)
    );
}

#[test]
fn renamed_feature_variants_keep_canonical_config_keys() {
    for (canonical_key, variant_shaped_key, feature) in [
        ("multi_agent", "collab", Feature::Collab),
        ("hooks", "codex_hooks", Feature::CodexHooks),
    ] {
        assert_eq!(
            feature_for_key(canonical_key),
            Some(feature),
            "{canonical_key}"
        );
        assert_eq!(
            feature_for_key(variant_shaped_key),
            None,
            "{variant_shaped_key}"
        );
    }
}

#[test]
fn multi_agent_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::Collab.stage(), Stage::Stable);
    assert_eq!(Feature::Collab.default_enabled(), true);
}

#[test]
fn enable_fanout_is_under_development() {
    assert_eq!(Feature::SpawnCsv.stage(), Stage::UnderDevelopment);
    assert_eq!(Feature::SpawnCsv.default_enabled(), false);
}

#[test]
fn enable_fanout_normalization_enables_multi_agent_one_way() {
    let mut enable_fanout_features = Features::with_defaults();
    enable_fanout_features.enable(Feature::SpawnCsv);
    enable_fanout_features.normalize_dependencies();
    assert_eq!(enable_fanout_features.enabled(Feature::SpawnCsv), true);
    assert_eq!(enable_fanout_features.enabled(Feature::Collab), true);

    let mut collab_features = Features::with_defaults();
    collab_features.enable(Feature::Collab);
    collab_features.normalize_dependencies();
    assert_eq!(collab_features.enabled(Feature::Collab), true);
    assert_eq!(collab_features.enabled(Feature::SpawnCsv), false);
}

#[test]
fn apps_require_feature_flag_and_chatgpt_auth() {
    let mut features = Features::with_defaults();
    assert!(!features.apps_enabled_for_auth(/*has_chatgpt_auth*/ false));

    features.enable(Feature::Apps);
    assert!(!features.apps_enabled_for_auth(/*has_chatgpt_auth*/ false));
    assert!(features.apps_enabled_for_auth(/*has_chatgpt_auth*/ true));
}

#[test]
fn from_sources_applies_base_profile_and_overrides() {
    let mut base_entries = BTreeMap::new();
    base_entries.insert("plugins".to_string(), true);
    let base_features = FeaturesToml {
        entries: base_entries,
        ..Default::default()
    };

    let mut profile_entries = BTreeMap::new();
    profile_entries.insert("code_mode_only".to_string(), true);
    let profile_features = FeaturesToml {
        entries: profile_entries,
        ..Default::default()
    };

    let features = Features::from_sources(
        FeatureConfigSource {
            features: Some(&base_features),
        },
        FeatureConfigSource {
            features: Some(&profile_features),
        },
        FeatureOverrides {
            web_search_request: Some(false),
        },
    );

    assert_eq!(features.enabled(Feature::Plugins), true);
    assert_eq!(features.enabled(Feature::CodeModeOnly), true);
    assert_eq!(features.enabled(Feature::CodeMode), true);
    assert_eq!(features.enabled(Feature::WebSearchRequest), false);
}

#[test]
fn multi_agent_v2_feature_config_deserializes_boolean_toggle() {
    let features: FeaturesToml = toml::from_str(
        r#"
multi_agent_v2 = true
"#,
    )
    .expect("features table should deserialize");

    assert_eq!(
        features.entries(),
        BTreeMap::from([("multi_agent_v2".to_string(), true)])
    );
    assert_eq!(features.multi_agent_v2, Some(FeatureToml::Enabled(true)));
}

#[test]
fn multi_agent_v2_feature_config_deserializes_table() {
    let features: FeaturesToml = toml::from_str(
        r#"
[multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 4
min_wait_timeout_ms = 2500
max_wait_timeout_ms = 120000
default_wait_timeout_ms = 30000
usage_hint_text = "Custom delegation guidance."
root_agent_usage_hint_text = "Root guidance."
subagent_usage_hint_text = "Subagent guidance."
multi_agent_mode_hint_text = "Custom mode guidance."
tool_namespace = "agents"
hide_spawn_agent_metadata = true
non_code_mode_only = true
allow_full_history_forks = true
"#,
    )
    .expect("features table should deserialize");

    assert_eq!(
        features.entries(),
        BTreeMap::from([("multi_agent_v2".to_string(), true)])
    );
    assert_eq!(
        features.multi_agent_v2,
        Some(crate::FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(true),
            max_concurrent_threads_per_session: Some(4),
            min_wait_timeout_ms: Some(2500),
            max_wait_timeout_ms: Some(120000),
            default_wait_timeout_ms: Some(30000),
            usage_hint_text: Some("Custom delegation guidance.".to_string()),
            root_agent_usage_hint_text: Some("Root guidance.".to_string()),
            subagent_usage_hint_text: Some("Subagent guidance.".to_string()),
            multi_agent_mode_hint_text: Some("Custom mode guidance.".to_string()),
            tool_namespace: Some("agents".to_string()),
            hide_spawn_agent_metadata: Some(true),
            non_code_mode_only: Some(true),
            allow_full_history_forks: Some(true),
        }))
    );
}

#[test]
fn multi_agent_v2_schema_uses_authoritative_policy_bounds() {
    let schema = schemars::schema_for!(crate::MultiAgentV2ConfigToml);
    let properties = schema
        .schema
        .object
        .as_ref()
        .expect("multi-agent v2 config should have an object schema")
        .properties
        .clone();
    let number_bounds = |field: &str| {
        let schemars::schema::Schema::Object(schema) = properties
            .get(field)
            .unwrap_or_else(|| panic!("missing schema for {field}"))
        else {
            panic!("{field} should have an integer schema");
        };
        let number = schema
            .number
            .as_ref()
            .unwrap_or_else(|| panic!("{field} should have numeric bounds"));
        (number.minimum, number.maximum)
    };

    assert_eq!(
        number_bounds("max_concurrent_threads_per_session"),
        (
            Some(crate::MULTI_AGENT_V2_MIN_CONCURRENT_THREADS_PER_SESSION as f64),
            None,
        )
    );
    for field in [
        "min_wait_timeout_ms",
        "max_wait_timeout_ms",
        "default_wait_timeout_ms",
    ] {
        assert_eq!(
            number_bounds(field),
            (
                Some(crate::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS as f64),
                Some(crate::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS as f64),
            )
        );
    }
    assert!(
        (crate::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS..=crate::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS)
            .contains(&crate::MULTI_AGENT_DEFAULT_WAIT_TIMEOUT_MS)
    );
}

#[test]
fn materialize_resolved_enabled_omits_internal_features_and_preserves_custom_config() {
    let mut features = Features::with_defaults();
    features.enable(Feature::CodeMode);
    features.enable(Feature::MultiAgentV2);
    features.enable(Feature::NetworkProxy);
    features.enable(Feature::RespectSystemProxy);

    let mut entries = BTreeMap::from([("custom_extension".to_string(), false)]);
    for spec in crate::FEATURES {
        if matches!(spec.stage, Stage::Internal) {
            entries.insert(spec.key.to_string(), true);
        }
    }
    let mut features_toml = FeaturesToml {
        multi_agent_v2: Some(FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(false),
            min_wait_timeout_ms: Some(2500),
            ..Default::default()
        })),
        network_proxy: Some(FeatureToml::Config(crate::NetworkProxyConfigToml {
            enabled: Some(false),
            proxy_url: Some("http://127.0.0.1:43128".to_string()),
            ..Default::default()
        })),
        entries,
        ..Default::default()
    };

    features_toml.materialize_resolved_enabled(&features);

    let entries = features_toml.entries();
    assert_eq!(entries.get("custom_extension"), Some(&false));
    for spec in crate::FEATURES {
        if matches!(spec.stage, Stage::Internal) {
            assert_eq!(entries.get(spec.key), None, "{}", spec.key);
        } else {
            assert_eq!(
                entries.get(spec.key),
                Some(&features.enabled(spec.id)),
                "{}",
                spec.key
            );
        }
    }
    assert_eq!(
        features_toml.multi_agent_v2,
        Some(FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(true),
            min_wait_timeout_ms: Some(2500),
            ..Default::default()
        }))
    );
    assert_eq!(
        features_toml.network_proxy,
        Some(FeatureToml::Config(crate::NetworkProxyConfigToml {
            enabled: Some(true),
            proxy_url: Some("http://127.0.0.1:43128".to_string()),
            ..Default::default()
        }))
    );
    let replayed = Features::from_sources(
        FeatureConfigSource {
            features: Some(&features_toml),
        },
        FeatureConfigSource::default(),
        FeatureOverrides::default(),
    );
    assert_eq!(replayed.enabled_features(), features.enabled_features());
}

#[test]
fn unstable_warning_event_only_mentions_enabled_under_development_features() {
    let mut configured_features = Table::new();
    configured_features.insert("enable_fanout".to_string(), TomlValue::Boolean(true));
    configured_features.insert("personality".to_string(), TomlValue::Boolean(true));
    configured_features.insert("unknown".to_string(), TomlValue::Boolean(true));

    let mut features = Features::with_defaults();
    features.enable(Feature::SpawnCsv);

    let warning = unstable_features_warning_event(
        Some(&configured_features),
        /*suppress_unstable_features_warning*/ false,
        &features,
        "/tmp/config.toml",
    )
    .expect("warning event");

    let EventMsg::Warning(WarningEvent { message }) = warning.msg else {
        panic!("expected warning event");
    };
    assert!(message.contains("enable_fanout"));
    assert!(!message.contains("personality"));
    assert!(message.contains("/tmp/config.toml"));
}

#[test]
fn unstable_warning_event_mentions_enabled_structured_under_development_feature() {
    let configured_features: Table = toml::from_str(
        r#"
current_time_reminder = { enabled = true }
enable_fanout = true
"#,
    )
    .expect("features table should deserialize");

    let mut features = Features::with_defaults();
    features.enable(Feature::CurrentTimeReminder);
    features.enable(Feature::SpawnCsv);

    let warning = unstable_features_warning_event(
        Some(&configured_features),
        /*suppress_unstable_features_warning*/ false,
        &features,
        "/tmp/config.toml",
    )
    .expect("warning event");

    let EventMsg::Warning(WarningEvent { message }) = warning.msg else {
        panic!("expected warning event");
    };
    assert_eq!(
        "Under-development features enabled: current_time_reminder, enable_fanout. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in /tmp/config.toml.".to_string(),
        message
    );
}

#[test]
fn unstable_warning_event_uses_canonical_keys() {
    let feature = Feature::CurrentTimeReminder;
    let mut configured_features = Table::new();
    configured_features.insert(feature.key().to_string(), TomlValue::Boolean(true));

    let mut features = Features::with_defaults();
    features.enable(feature);

    let warning = unstable_features_warning_event(
        Some(&configured_features),
        /*suppress_unstable_features_warning*/ false,
        &features,
        "/tmp/config.toml",
    )
    .expect("warning event");
    let EventMsg::Warning(WarningEvent { message }) = warning.msg else {
        panic!("expected warning event");
    };
    let expected = format!(
        "Under-development features enabled: {}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in /tmp/config.toml.",
        feature.key()
    );
    assert_eq!(message, expected);
}

#[test]
fn unified_exec_is_stable_and_enabled_by_default() {
    assert_eq!(Feature::UnifiedExec.stage(), Stage::Stable);
    assert_eq!(Feature::UnifiedExec.default_enabled(), true);
    assert_eq!(feature_for_key("unified_exec"), Some(Feature::UnifiedExec));
}

#[test]
fn known_delta_store_is_enabled_by_default() {
    assert_eq!(Feature::KnownDeltaStore.stage(), Stage::Stable);
    assert_eq!(Feature::KnownDeltaStore.default_enabled(), true);
    assert_eq!(
        feature_for_key("known_delta_store"),
        Some(Feature::KnownDeltaStore)
    );
}
#[test]
fn windows_only_feature_catalog_excludes_unix_shell_backends() {
    assert!(super::feature_for_key("shell_zsh_fork").is_none());
    assert!(super::feature_for_key("unified_exec_zsh_fork").is_none());
}
