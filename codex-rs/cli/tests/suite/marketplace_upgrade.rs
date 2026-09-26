use anyhow::Result;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

use super::codex_command;

#[tokio::test]
async fn marketplace_upgrade_runs_under_plugin() -> Result<()> {
    let codex_home = TempDir::new()?;

    codex_command(codex_home.path())?
        .args(["plugin", "marketplace", "upgrade"])
        .assert()
        .success()
        .stdout(contains("No configured Git marketplaces to upgrade."));

    Ok(())
}

#[tokio::test]
async fn marketplace_upgrade_json_prints_upgrade_outcome() -> Result<()> {
    let codex_home = TempDir::new()?;

    let assert = codex_command(codex_home.path())?
        .args(["plugin", "marketplace", "upgrade", "--json"])
        .assert()
        .success();
    let stdout = assert.get_output().stdout.as_slice();
    let actual: serde_json::Value = serde_json::from_slice(stdout)?;

    assert_eq!(
        actual,
        json!({
            "selectedMarketplaces": [],
            "upgradedRoots": [],
            "errors": [],
        })
    );

    Ok(())
}

#[tokio::test]
async fn marketplace_upgrade_no_longer_runs_at_top_level() -> Result<()> {
    let codex_home = TempDir::new()?;

    codex_command(codex_home.path())?
        .args(["marketplace", "upgrade"])
        .assert()
        .failure()
        .stderr(contains("unrecognized subcommand 'upgrade'"));

    Ok(())
}

#[test]
fn partial_marketplace_upgrade_prints_successes_and_errors() -> Result<()> {
    let repo = TempDir::new()?;
    let manifest = repo.path().join(".agents/plugins");
    std::fs::create_dir_all(&manifest)?;
    std::fs::write(
        manifest.join("marketplace.json"),
        r#"{"name":"good","plugins":[]}"#,
    )?;
    for args in [
        vec!["init"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Codex Test",
            "-c",
            "user.email=codex-test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial",
        ],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(&args)
            .output()?;
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for json_output in [false, true] {
        let home = TempDir::new()?;
        let good = url::Url::from_directory_path(repo.path()).unwrap();
        let bad = url::Url::from_directory_path(home.path().join("missing-repo")).unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                r#"
[marketplaces.bad]
source_type = "git"
source = "{bad}"
[marketplaces.good]
source_type = "git"
source = "{good}"
"#
            ),
        )?;
        let mut command = codex_command(home.path())?;
        command.args(["plugin", "marketplace", "upgrade"]);
        if json_output {
            command.arg("--json");
        }
        let output = command
            .assert()
            .failure()
            .stderr(contains("Failed to upgrade marketplace `bad`"));
        if json_output {
            let value: serde_json::Value = serde_json::from_slice(&output.get_output().stdout)?;
            assert_eq!(value["selectedMarketplaces"], json!(["bad", "good"]));
            assert_eq!(value["errors"].as_array().unwrap().len(), 1);
            assert_eq!(value["errors"][0]["marketplaceName"], "bad");
            let roots = value["upgradedRoots"].as_array().unwrap();
            assert_eq!(roots.len(), 1);
            assert!(
                Path::new(roots[0].as_str().unwrap())
                    .join(".agents/plugins/marketplace.json")
                    .is_file()
            );
        } else {
            output
                .stdout(contains("Upgraded 1 marketplace(s)."))
                .stdout(contains("Installed marketplace root:"));
        }
    }
    Ok(())
}
