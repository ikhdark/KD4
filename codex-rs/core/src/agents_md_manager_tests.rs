use super::*;
use crate::config::ConfigBuilder;
use crate::session::turn_context::TurnEnvironment;
use codex_exec_server::Environment;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use tempfile::TempDir;
use toml::Value as TomlValue;

async fn config_for(root: &TempDir) -> Config {
    let codex_home = tempfile::tempdir().expect("codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("test config");
    config.cwd = AbsolutePathBuf::from_absolute_path(root.path()).expect("absolute root");
    config.project_doc_max_bytes = 4_096;
    config
}

fn environment_snapshot(cwd: &AbsolutePathBuf, generation: u64) -> TurnEnvironmentSnapshot {
    TurnEnvironmentSnapshot {
        generation,
        turn_environments: vec![TurnEnvironment::new(
            "local".to_string(),
            Arc::new(Environment::default_for_tests()),
            PathUri::from_abs_path(cwd),
            /*shell*/ None,
        )],
        starting: Vec::new(),
    }
}

#[tokio::test]
async fn stable_refresh_reuses_loaded_arc_and_semantic_digest() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "stable instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 3);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    let first = manager.get_loaded().await.expect("first load");
    manager.refresh(&config, &environments).await;
    let second = manager.get_loaded().await.expect("cached load");

    assert!(Arc::ptr_eq(&first, &second));
    let expected_digest: [u8; 32] = Sha256::digest(first.text().as_bytes()).into();
    assert_eq!(*first.semantic_digest(), expected_digest);
}

#[tokio::test]
async fn content_and_missing_higher_precedence_file_invalidate_cache() {
    let root = tempfile::tempdir().expect("workspace");
    let agents = root.path().join("AGENTS.md");
    let override_path = root.path().join("AGENTS.override.md");
    fs::write(&agents, "version one").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 0);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    let first = manager.get_loaded().await.expect("first load");
    fs::write(&agents, "version two").expect("replace same-size contents");
    manager.refresh(&config, &environments).await;
    let changed = manager.get_loaded().await.expect("changed load");
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(changed.text(), "version two");
    assert_ne!(first.semantic_digest(), changed.semantic_digest());

    fs::write(&override_path, "local override").expect("create override");
    manager.refresh(&config, &environments).await;
    let overridden = manager.get_loaded().await.expect("override load");
    assert!(!Arc::ptr_eq(&changed, &overridden));
    assert_eq!(overridden.text(), "local override");
}

#[tokio::test]
async fn names_and_limits_are_cache_dependencies() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("WORKFLOW.md"), "fallback instructions").expect("write fallback");
    let mut config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 1);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    assert!(manager.get_loaded().await.is_none());

    config.project_doc_fallback_filenames = vec!["WORKFLOW.md".to_string()];
    manager.refresh(&config, &environments).await;
    let fallback = manager.get_loaded().await.expect("fallback load");
    assert_eq!(fallback.text(), "fallback instructions");

    config.project_doc_max_bytes = 8;
    manager.refresh(&config, &environments).await;
    let truncated = manager.get_loaded().await.expect("truncated load");
    assert!(!Arc::ptr_eq(&fallback, &truncated));
    assert_eq!(truncated.text(), "fallback");
}

#[tokio::test]
async fn effective_marker_configuration_invalidates_cached_discovery() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join(".codex-root"), "").expect("write marker");
    fs::write(root.path().join("AGENTS.md"), "root instructions").expect("write root doc");
    let nested = root.path().join("nested");
    fs::create_dir(&nested).expect("create nested directory");
    fs::write(nested.join("AGENTS.md"), "nested instructions").expect("write nested doc");

    let mut default_config = config_for(&root).await;
    default_config.cwd = AbsolutePathBuf::from_absolute_path(&nested).expect("absolute nested");
    let codex_home = tempfile::tempdir().expect("marker codex home");
    let mut marker_config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "project_root_markers".to_string(),
            TomlValue::Array(vec![TomlValue::String(".codex-root".to_string())]),
        )])
        .build()
        .await
        .expect("marker config");
    marker_config.cwd = default_config.cwd.clone();
    marker_config.project_doc_max_bytes = default_config.project_doc_max_bytes;
    let environments = environment_snapshot(&default_config.cwd, 4);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&default_config, &environments).await;
    let nested_only = manager.get_loaded().await.expect("nested load");
    assert_eq!(nested_only.text(), "nested instructions");

    manager.refresh(&marker_config, &environments).await;
    let with_root = manager.get_loaded().await.expect("root-aware load");
    assert!(!Arc::ptr_eq(&nested_only, &with_root));
    assert_eq!(
        with_root.text(),
        "root instructions\n\nnested instructions"
    );
}

#[tokio::test]
async fn identical_paths_on_distinct_filesystems_do_not_share_cache_entries() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let first_environments = environment_snapshot(&config.cwd, 9);
    let second_environments = environment_snapshot(&config.cwd, 9);
    let first_key = AgentsMdCacheKey::capture(&config, &first_environments);
    let second_key = AgentsMdCacheKey::capture(&config, &second_environments);
    assert_ne!(first_key, second_key);

    let manager = AgentsMdManager::new(/*user_instructions*/ None);
    manager.refresh(&config, &first_environments).await;
    let first = manager.get_loaded().await.expect("first filesystem load");
    manager.refresh(&config, &second_environments).await;
    let second = manager.get_loaded().await.expect("second filesystem load");
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(first.text(), second.text());

    let mut next_generation = second_environments.clone();
    next_generation.generation += 1;
    assert_ne!(
        AgentsMdCacheKey::capture(&config, &second_environments),
        AgentsMdCacheKey::capture(&config, &next_generation)
    );
}
