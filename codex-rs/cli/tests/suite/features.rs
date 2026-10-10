use anyhow::Result;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::codex_command;

#[test]
fn strict_config_rejects_unknown_config_override() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--strict-config", "-c", "foo=bar", "mcp-server"])
        .assert()
        .failure()
        .stderr(contains("unknown configuration field"));

    Ok(())
}

#[test]
fn strict_config_is_not_supported_for_cloud_command() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--strict-config", "-c", "foo=bar", "cloud", "list"])
        .assert()
        .failure()
        .stderr(contains(
            "`--strict-config` is not supported for `codex cloud`",
        ));

    Ok(())
}

#[tokio::test]
async fn features_enable_and_disable_persist_without_losing_other_settings() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(codex_home.path().join("config.toml"), "model = \"test-model\"\n")?;
    for (command, feature, enabled, message) in [
        ("enable", "unified_exec", true, "Enabled"),
        ("disable", "shell_tool", false, "Disabled"),
    ] {
        codex_command(codex_home.path())?
            .args(["features", command, feature])
            .assert()
            .success()
            .stdout(contains(format!("{message} feature `{feature}` in config.toml.")));
        let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(codex_home.path().join("config.toml"))?,
        )?;
        assert_eq!(config["model"].as_str(), Some("test-model"));
        assert_eq!(config["features"][feature].as_bool(), Some(enabled));
        assert_eq!(config["features"]["unified_exec"].as_bool(), Some(true));
    }
    Ok(())
}

#[tokio::test]
async fn features_enable_under_development_feature_prints_warning() -> Result<()> {
    let codex_home = TempDir::new()?;

    // Control: enabling a stable feature does not print the warning.
    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["features", "enable", "shell_tool"])
        .assert()
        .success()
        .stderr(contains("Under-development features enabled").not());

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["features", "enable", "runtime_metrics"])
        .assert()
        .success()
        .stderr(contains(
            "Under-development features enabled: runtime_metrics.",
        ));

    let config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    assert!(config.contains("runtime_metrics = true"));

    Ok(())
}

#[tokio::test]
async fn features_enable_rejects_legacy_aliases() -> Result<()> {
    for alias in ["telepathy", "request_permissions"] {
        let codex_home = TempDir::new()?;

        let mut cmd = codex_command(codex_home.path())?;
        cmd.args(["features", "enable", alias])
            .assert()
            .failure()
            .stderr(contains(format!("Unknown feature flag: {alias}")));

        assert!(!codex_home.path().join("config.toml").exists());
    }

    Ok(())
}

#[tokio::test]
async fn features_list_is_sorted_alphabetically_by_feature_name() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    let output = cmd
        .args(["features", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output)?;

    let actual_names = stdout
        .lines()
        .map(|line| {
            line.split_once("  ")
                .map(|(name, _)| name.trim_end().to_string())
                .expect("feature list output should contain aligned columns")
        })
        .collect::<Vec<_>>();
    let mut expected_names = codex_features::user_settable_features()
        .map(|feature| feature.key.to_string())
        .collect::<Vec<_>>();
    assert!(!expected_names.is_empty());
    expected_names.sort();

    assert_eq!(actual_names, expected_names);

    Ok(())
}
