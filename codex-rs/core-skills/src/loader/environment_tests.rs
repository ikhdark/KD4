use std::fs;

use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::Product;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use crate::model::SkillDependencies;
use crate::model::SkillPolicy;
use crate::model::SkillToolDependency;

use super::EnvironmentSkillMetadata;
use super::load_environment_skills_from_root;

#[tokio::test]
async fn reports_optional_metadata_failures_without_dropping_environment_skill() {
    let root = tempdir().unwrap();
    let skill_dir = root.path().join("demo");
    fs::create_dir_all(skill_dir.join("agents")).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: valid skill\n---\n",
    )
    .unwrap();
    let metadata_path = skill_dir.join("agents/openai.yaml");
    let root_uri = PathUri::from_host_native_path(root.path()).unwrap();
    for (content, expected) in [
        ("dependencies: [", "invalid metadata"),
        (
            "dependencies:\n  tools:\n    - type: mcp\n",
            "dependencies.tools.value: value is missing",
        ),
    ] {
        fs::write(&metadata_path, content).unwrap();
        let outcome = load_environment_skills_from_root(LOCAL_FS.as_ref(), &root_uri, None).await;
        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.skills[0].name, "demo");
        assert_eq!(outcome.skills[0].dependencies, None);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].contains(expected),
            "{:?}",
            outcome.warnings
        );
        assert!(outcome.warnings[0].contains("openai.yaml"));
    }
    fs::remove_file(metadata_path).unwrap();
    let outcome = load_environment_skills_from_root(LOCAL_FS.as_ref(), &root_uri, None).await;
    assert_eq!(outcome.skills.len(), 1);
    assert_eq!(outcome.warnings, Vec::<String>::new());
}

#[tokio::test]
async fn loads_plugin_namespace_dependencies_and_policy() {
    let root = tempdir().expect("tempdir");
    let skill_dir = root.path().join("skills/deploy");
    fs::create_dir_all(root.path().join(".codex-plugin")).expect("manifest dir");
    fs::create_dir_all(skill_dir.join("agents")).expect("metadata dir");
    fs::write(
        root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"demo-plugin"}"#,
    )
    .expect("manifest");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the service.\n---\n",
    )
    .expect("skill");
    fs::write(
        skill_dir.join("agents/openai.yaml"),
        r#"
dependencies:
  tools:
    - type: mcp
      value: deploy-server
      description: Deploy MCP
policy:
  allow_implicit_invocation: false
  products: [codex]
"#,
    )
    .expect("metadata");

    let root_uri = PathUri::from_host_native_path(root.path()).expect("root URI");
    let outcome =
        load_environment_skills_from_root(LOCAL_FS.as_ref(), &root_uri, Some(Product::Codex)).await;

    assert_eq!(
        outcome.skills,
        vec![EnvironmentSkillMetadata {
            path_to_skills_md: PathUri::from_host_native_path(skill_dir.join("SKILL.md"),).unwrap(),
            name: "demo-plugin:deploy".to_string(),
            description: "Deploy the service.".to_string(),
            short_description: None,
            dependencies: Some(SkillDependencies {
                tools: vec![SkillToolDependency {
                    r#type: "mcp".to_string(),
                    value: "deploy-server".to_string(),
                    description: Some("Deploy MCP".to_string()),
                    transport: None,
                    command: None,
                    url: None,
                }],
            }),
            policy: Some(SkillPolicy {
                allow_implicit_invocation: Some(false),
                products: vec![Product::Codex],
            }),
        }]
    );
    let filtered =
        load_environment_skills_from_root(LOCAL_FS.as_ref(), &root_uri, Some(Product::Chatgpt))
            .await;
    assert!(filtered.skills.is_empty());
}
