//! Transport only: KDA owns semantic analysis, evidence, migrations and verification.
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::Write, path::PathBuf};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository: PathBuf,
    pub queries: Vec<Query>,
    #[serde(default)]
    pub configuration: Configuration,
    #[serde(default)]
    pub compiler_check: bool,
    pub state_directory: Option<PathBuf>,
    pub migration_id: Option<String>,
    #[serde(default)]
    pub reviewed_consumers: BTreeSet<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    pub target: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub build_scripts: bool,
    #[serde(default)]
    pub procedural_macros: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

pub fn run(request: Request) -> anyhow::Result<Value> {
    anyhow::ensure!(
        !request.queries.is_empty()
            && request.queries.len() <= 16
            && request.queries.iter().all(|q| q.line > 0 && q.column > 0),
        "provide 1-16 queries with one-based lines and UTF-16 columns"
    );
    let root = fs::canonicalize(&request.repository)?;
    let mut arguments = serde_json::to_value(&request)?;
    arguments.as_object_mut().unwrap().remove("repository");
    let operation = json!({"op":"semantic_context","request":arguments});
    let mut input = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut input, &operation)?;
    input.flush()?;
    // Host configuration, never a repository-provided executable override.
    let executable = std::env::var_os("CODEX_KDA_EXECUTABLE").unwrap_or_else(|| "cargo-kda".into());
    let output = crate::command(&executable)
        .arg("op").arg("--root").arg(&root).arg("--request").arg(input.path())
        // A cold multi-crate workspace can exceed two minutes before queries
        // run. Keep a finite budget that also accommodates compiler verification.
        .args(["--deadline-ms", "600000"])
        .current_dir(&root).output()
        .context("KDA is required: cargo-kda must be on PATH or CODEX_KDA_EXECUTABLE must identify a compatible binary")?;
    decode(&output.stdout, &output.stderr, output.status.success())
}

fn decode(stdout: &[u8], stderr: &[u8], process_success: bool) -> anyhow::Result<Value> {
    let envelope: Value = serde_json::from_slice(stdout).with_context(|| {
        format!(
            "KDA returned invalid JSON: {}",
            String::from_utf8_lossy(stderr)
        )
    })?;
    anyhow::ensure!(
        envelope["operation"] == "semantic_context",
        "KDA semantic_context is unavailable or rejected: {envelope}"
    );
    let report = &envelope["report"];
    anyhow::ensure!(
        report["contract_version"] == 1 && report["engine"] == "kda",
        "incompatible KDA semantic-context contract; update KDA before using this tool"
    );
    anyhow::ensure!(
        report["success"].is_boolean() && report["results"].is_array(),
        "incomplete KDA semantic-context response"
    );
    anyhow::ensure!(
        report["success"] != true || (process_success && envelope["ok"] == true),
        "KDA failed after producing a report; verification was not accepted: {envelope}"
    );
    Ok(report.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kda_contract_rejects_old_provider_and_preserves_verification_failure() {
        let valid = json!({"operation":"semantic_context","ok":true,"report":{
            "contract_version":1,"engine":"kda","success":true,"results":[]}});
        let encoded = serde_json::to_vec(&valid).unwrap();
        assert_eq!(decode(&encoded, b"", true).unwrap(), valid["report"]);
        assert!(decode(&encoded, b"", false).is_err());
        let mut wrong = valid.clone();
        wrong["report"]["engine"] = json!("rust-analyzer");
        assert!(decode(&serde_json::to_vec(&wrong).unwrap(), b"", true).is_err());
        wrong = valid.clone();
        wrong["report"]["contract_version"] = json!(2);
        assert!(decode(&serde_json::to_vec(&wrong).unwrap(), b"", true).is_err());
        for (field, value) in [("ok", json!(false)), ("operation", json!("query"))] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(decode(&serde_json::to_vec(&invalid).unwrap(), b"", true).is_err());
        }
        for field in ["success", "results"] {
            let mut incomplete = valid.clone();
            incomplete["report"].as_object_mut().unwrap().remove(field);
            assert!(decode(&serde_json::to_vec(&incomplete).unwrap(), b"", true).is_err());
        }
        let mut failed = valid;
        failed["ok"] = json!(false);
        failed["report"]["success"] = json!(false);
        failed["report"]["compiler"] =
            json!({"success":false,"diagnostics":[{"message":"type mismatch"}]});
        assert_eq!(
            decode(&serde_json::to_vec(&failed).unwrap(), b"", false).unwrap(),
            failed["report"]
        );
        assert!(
            decode(
                br#"{"operation":"error","error":"stale analyzer"}"#,
                b"",
                false
            )
            .is_err()
        );
        assert!(decode(b"not JSON", b"provider failed", false).is_err());
    }

    #[test]
    fn configured_compiler_and_persistent_migration_are_wired_end_to_end() {
        let dir = tempfile::Builder::new().prefix("semantic transport '$ Ω ").tempdir().unwrap();
        let root = dir.path().join("repo");
        let workspace = root.join("codex-rs");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("Cargo.toml"), "[package]\nname='configured_fixture'\nversion='0.1.0'\nedition='2021'\n[features]\nselected=[]\n").unwrap();
        let source = "#[cfg(feature=\"selected\")]\npub fn answer() -> u32 { 42 }\n#[cfg(feature=\"selected\")]\npub fn caller() -> u32 { answer() }\n";
        fs::write(workspace.join("src/lib.rs"), source).unwrap();
        let status = crate::command("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(status.success());
        let query = |reviewed: BTreeSet<String>, packages: Vec<String>| {
            let request = Request {
                repository: root.clone(),
                queries: vec![Query {
                    path: "codex-rs/src/lib.rs".into(),
                    line: 2,
                    column: 9,
                }],
                configuration: Configuration {
                    features: vec!["selected".into()],
                    packages,
                    ..Default::default()
                },
                compiler_check: true,
                state_directory: Some(dir.path().join("state")),
                migration_id: Some("representation".into()),
                reviewed_consumers: reviewed,
            };
            crate::run("semantic_context", &serde_json::to_string(&request).unwrap())
            .unwrap()
        };
        let initial = query(BTreeSet::new(), vec!["configured_fixture".into()]);
        assert_eq!(initial["engine"], "kda");
        assert_eq!(initial["contract_version"], 1);
        assert!(initial["analysis_world"].is_string());
        assert_eq!(fs::canonicalize(initial["cargo_workspace"].as_str().unwrap()).unwrap(), fs::canonicalize(&workspace).unwrap());
        assert_eq!(initial["compiler"]["success"], true, "{initial}");
        assert_eq!(initial["migration"]["complete"], false);
        let consumers = initial["migration"]["consumers"].as_array().unwrap();
        assert!(
            !consumers.is_empty(),
            "configured reference must be discovered: {initial}"
        );
        assert!(
            initial["results"][0]["source_bundle"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["complete"] == true && s["edit_handle"].is_string())
        );
        let handles = initial["results"][0]["source_bundle"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["edit_handle"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            handles.len(),
            handles.iter().collect::<BTreeSet<_>>().len(),
            "return each complete unit once"
        );
        assert!(consumers.iter().all(|c| c["source"].get("text").is_none()));
        let reviewed = consumers
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect();
        let complete = query(reviewed, Vec::new());
        assert_eq!(complete["analysis_world"], initial["analysis_world"]);
        assert_eq!(complete["migration"]["complete"], true, "{complete}");
        fs::write(
            workspace.join("src/lib.rs"),
            source.replace("{ 42 }", "{ false }"),
        )
        .unwrap();
        let failed = query(BTreeSet::new(), Vec::new());
        assert_eq!(failed["success"], false, "{failed}");
        assert_eq!(failed["migration"]["complete"], false);
        assert!(
            !failed["compiler"]["checks"][0]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn real_compiler_resolves_exact_path_dependency_and_callers() {
        // Deliberately mandatory: this gate proves actual semantic resolution.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("dep/src")).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname='semantic_fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nfixture_dep={path='dep'}\n").unwrap();
        fs::write(
            dir.path().join("dep/Cargo.toml"),
            "[package]\nname='fixture_dep'\nversion='0.7.3'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("dep/src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn caller() -> u32 { fixture_dep::answer() }\n",
        )
        .unwrap();
        let output = crate::command("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = run(Request {
            repository: dir.path().into(),
            queries: vec![Query {
                path: "src/lib.rs".into(),
                line: 1,
                column: 40,
            }],
            configuration: Configuration::default(),
            compiler_check: false,
            state_directory: None,
            migration_id: None,
            reviewed_consumers: BTreeSet::new(),
        })
        .unwrap();
        assert_eq!(result["results"][0]["resolved"], true, "{result}");
        assert!(
            result["results"][0]["definitions"]
                .to_string()
                .contains("dep/src/lib.rs"),
            "{result}"
        );
        assert!(
            result["results"][0]["hover"].to_string().contains("u32"),
            "{result}"
        );
        assert!(
            result["results"][0]["caller_count"].as_u64().unwrap() >= 1,
            "{result}"
        );
        assert!(
            result["results"][0]["source_bundle"]
                .to_string()
                .contains("42"),
            "{result}"
        );
    }
}
