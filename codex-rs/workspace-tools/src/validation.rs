//! Scoped Cargo execution with warm, leased output lanes and dependency-bound receipts.
//! The worker runs inside the normal command sandbox and owns its children until
//! they exit. A failed or changing-input run is never reusable evidence.
use anyhow::Context;
use anyhow::ensure;
use fs2::FileExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository: PathBuf,
    pub cache_directory: PathBuf,
    #[serde(default)]
    pub cache_identity: Option<PathBuf>,
    pub action: Action,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub allow_full_suite: bool,
    #[serde(default)]
    pub force_fresh: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Plan,
    Run,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    /// Cargo arguments, without the executable; no shell expressions.
    pub args: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    fingerprint: String,
    result: Value,
}

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn output(program: &str, args: &[&str], root: &Path) -> anyhow::Result<String> {
    let result = crate::command(program)
        .args(args)
        .current_dir(root)
        .output()?;
    ensure!(
        result.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?)
}

fn metadata(root: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&output(
        "cargo",
        &[
            "metadata",
            "--format-version=1",
            "--locked",
            "--offline",
            "--all-features",
        ],
        root,
    )?)?)
}

fn validate_check(check: &Check, allow_full_suite: bool) -> anyhow::Result<()> {
    ensure!(
        !check.id.is_empty() && check.id.len() <= 128,
        "check id must have 1-128 bytes"
    );
    let args = &check.args;
    ensure!(
        matches!(
            args.first().map(String::as_str),
            Some("check" | "test" | "clippy")
        ),
        "only cargo check, test and clippy are supported"
    );
    ensure!(
        !args.iter().any(|a| a == "--target-dir"
            || a.starts_with("--target-dir=")
            || a == "--config"
            || a.starts_with("--config=")
            || a.starts_with('+')),
        "build output and toolchain overrides must not bypass the worker's owned lane"
    );
    ensure!(
        !args
            .iter()
            .any(|a| a == "--manifest-path" || a.starts_with("--manifest-path=")),
        "repository selects the manifest; alternate manifest paths are not allowed"
    );
    ensure!(
        !args.iter().any(|a| a.starts_with("--message-format")
            || a.starts_with("--format")
            || matches!(a.as_str(), "--quiet" | "-q" | "--list" | "--no-run")),
        "validation requires structured compiler messages and executed tests with full test names"
    );
    if args[0] == "test" && !allow_full_suite {
        let explicit_target = args
            .iter()
            .any(|a| matches!(a.as_str(), "--test" | "--lib" | "--bin" | "--doc"));
        let filtered = args
            .windows(2)
            .any(|a| a[0] == "--" && !a[1].starts_with('-'));
        ensure!(
            explicit_target || filtered,
            "unscoped cargo test is outside this validation contract; specify a target/filter or explicitly authorize allow_full_suite"
        );
        ensure!(
            !args
                .iter()
                .any(|a| matches!(a.as_str(), "--workspace" | "--all" | "--all-targets")),
            "workspace-wide tests require allow_full_suite"
        );
    }
    Ok(())
}

fn selected_packages(meta: &Value, check: &Check) -> anyhow::Result<BTreeSet<String>> {
    let packages = meta["packages"]
        .as_array()
        .context("Cargo packages missing")?;
    let names = check
        .args
        .windows(2)
        .filter(|a| a[0] == "-p" || a[0] == "--package")
        .map(|a| a[1].clone())
        .chain(
            check
                .args
                .iter()
                .filter_map(|a| a.strip_prefix("--package=").map(str::to_owned)),
        )
        .collect::<BTreeSet<_>>();
    let mut ids = if names.is_empty() {
        meta["workspace_members"]
            .as_array()
            .context("Cargo workspace missing")?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    } else {
        let mut ids = BTreeSet::new();
        for name in names {
            let found = packages
                .iter()
                .filter(|p| p["name"] == name || p["id"] == name)
                .collect::<Vec<_>>();
            ensure!(
                found.len() == 1,
                "package selector {name} is ambiguous or unresolved; use a concrete package name"
            );
            ids.insert(found[0]["id"].as_str().context("package id")?.to_owned());
        }
        ids
    };
    // Include transitive build/dev dependencies. Cargo's resolved graph is an
    // overapproximation across targets/features, which trades hits for safety.
    loop {
        let old = ids.len();
        for node in meta["resolve"]["nodes"]
            .as_array()
            .context("resolved dependency graph missing")?
        {
            if ids.contains(node["id"].as_str().unwrap_or_default()) {
                for dep in node["dependencies"].as_array().context("dependency list")? {
                    ids.insert(dep.as_str().context("dependency id")?.to_owned());
                }
            }
        }
        if old == ids.len() {
            break;
        }
    }
    Ok(ids)
}

fn hash_tree(root: &Path, hash: &mut Sha256, count: &mut usize) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if matches!(name.to_str(), Some("target" | ".git" | ".codex-validation")) {
            continue;
        }
        let ty = entry.file_type()?;
        ensure!(
            !ty.is_symlink(),
            "validation dependency contains a symlink: {}",
            entry.path().display()
        );
        hash.update(name.to_string_lossy().as_bytes());
        hash.update([0]);
        if ty.is_dir() {
            hash.update(b"directory");
            hash_tree(&entry.path(), hash, count)?;
            hash.update(b"end-directory\0");
        } else if ty.is_file() {
            *count += 1;
            ensure!(
                *count <= 100_000 && entry.metadata()?.len() <= 64 * 1024 * 1024,
                "dependency fingerprint exceeds its bounded file limit"
            );
            let bytes = fs::read(entry.path())?;
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    Ok(())
}

fn fingerprint(
    root: &Path,
    meta: &Value,
    check: &Check,
    toolchain: &str,
) -> anyhow::Result<String> {
    let ids = selected_packages(meta, check)?;
    let mut hash = Sha256::new();
    hash.update(b"codex-validation-v2");
    hash.update(serde_json::to_vec(check)?);
    hash.update(toolchain.as_bytes());
    let mut count = 0;
    for package in meta["packages"].as_array().context("packages")? {
        if ids.contains(package["id"].as_str().unwrap_or_default()) {
            hash.update(package["name"].as_str().context("name")?.as_bytes());
            hash.update(package["version"].as_str().context("version")?.as_bytes());
            hash_tree(
                Path::new(package["manifest_path"].as_str().context("manifest path")?)
                    .parent()
                    .context("manifest parent")?,
                &mut hash,
                &mut count,
            )?;
        }
    }
    // Workspace resolution/configuration changes invalidate every affected check.
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config",
        ".cargo/config.toml",
    ] {
        hash.update(name.as_bytes());
        match fs::read(root.join(name)) {
            Ok(bytes) => {
                hash.update([1]);
                hash.update(bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => hash.update([0]),
            Err(e) => return Err(e.into()),
        }
    }
    // Cargo also reads configuration above the selected manifest and in CARGO_HOME.
    // Those inputs live outside package trees and must invalidate saved passes.
    let mut config_roots = root
        .ancestors()
        .skip(1)
        .map(|p| p.join(".cargo"))
        .collect::<BTreeSet<_>>();
    if let Some(home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|p| PathBuf::from(p).join(".cargo"))
        })
    {
        config_roots.insert(home);
    }
    for directory in config_roots {
        for name in ["config", "config.toml"] {
            match fs::read(directory.join(name)) {
                Ok(bytes) => {
                    hash.update(name);
                    hash.update((bytes.len() as u64).to_le_bytes());
                    hash.update(bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    // Never persist environment values in receipts; only their digest.
    let env = std::env::vars_os()
        .filter(|(key, _)| {
            !matches!(
                key.to_str(),
                Some(
                    "CARGO_TARGET_DIR"
                        | "CARGO_BUILD_BUILD_DIR"
                        | "CODEX_VALIDATION_SOURCE_REVISION"
                        | "PWD"
                        | "OLDPWD"
                )
            )
        })
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    hash.update(serde_json::to_vec(&env)?);
    Ok(format!("{:x}", hash.finalize()))
}

fn changed_packages(root: &Path, meta: &Value) -> anyhow::Result<(Vec<String>, BTreeSet<String>)> {
    let mut names = output(
        "git",
        &["diff", "--relative", "--name-only", "-z", "HEAD", "--"],
        root,
    )?
    .split('\0')
    .filter(|name| !name.is_empty())
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    names.extend(
        output(
            "git",
            &["ls-files", "--others", "--exclude-standard", "-z"],
            root,
        )?
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned),
    );
    let packages = meta["packages"].as_array().context("packages")?;
    let members = meta["workspace_members"].as_array().context("members")?;
    let global = names.iter().any(|p| {
        matches!(
            p.as_str(),
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
        ) || p.starts_with(".cargo/")
    });
    let mut affected = BTreeSet::new();
    for package in packages.iter().filter(|p| members.contains(&p["id"])) {
        let dir = Path::new(package["manifest_path"].as_str().context("manifest")?)
            .parent()
            .context("parent")?;
        let dir = fs::canonicalize(dir)?;
        if global || names.iter().any(|p| root.join(p).starts_with(&dir)) {
            affected.insert(package["id"].as_str().context("id")?.to_owned());
        }
    }
    loop {
        let old = affected.len();
        for node in meta["resolve"]["nodes"].as_array().context("nodes")? {
            if members.contains(&node["id"])
                && node["dependencies"]
                    .as_array()
                    .context("dependencies")?
                    .iter()
                    .any(|d| affected.contains(d.as_str().unwrap_or_default()))
            {
                affected.insert(node["id"].as_str().context("node id")?.to_owned());
            }
        }
        if old == affected.len() {
            break;
        }
    }
    Ok((names.into_iter().collect(), affected))
}

pub fn run(request: Request) -> anyhow::Result<Value> {
    let root = fs::canonicalize(&request.repository)?;
    let meta = metadata(&root)?;
    if matches!(request.action, Action::Plan) {
        let (changed_paths, affected) = changed_packages(&root, &meta)?;
        let mut checks = Vec::new();
        let mut test_targets = Vec::new();
        for package in meta["packages"].as_array().context("packages")? {
            if !affected.contains(package["id"].as_str().unwrap_or_default()) {
                continue;
            }
            let name = package["name"].as_str().context("package name")?;
            checks.push(Check {
                id: format!("check:{name}"),
                args: vec![
                    "check".into(),
                    "--locked".into(),
                    "-p".into(),
                    name.into(),
                    "--tests".into(),
                ],
            });
            for target in package["targets"].as_array().context("targets")? {
                if target["test"] == true {
                    test_targets.push(json!({"package":name,"name":target["name"],"kind":target["kind"],"source":target["src_path"]}));
                }
            }
        }
        return Ok(
            json!({"changed_paths":changed_paths,"checks":checks,"affected_test_targets":test_targets,
            "instruction":"Choose affected test targets/filters from this inventory and add required repository checks before run. No tests have run. Broad suites require explicit allow_full_suite; user constraints take precedence."}),
        );
    }
    ensure!(
        !request.checks.is_empty() && request.checks.len() <= 64,
        "provide 1-64 scoped checks"
    );
    let mut ids = BTreeSet::new();
    for check in &request.checks {
        validate_check(check, request.allow_full_suite)?;
        ensure!(ids.insert(check.id.clone()), "duplicate check id");
    }
    let toolchain = format!(
        "{}\n{}",
        output("rustc", &["-vV"], &root)?,
        output("cargo", &["-V"], &root)?
    );
    // A lane is stable across edits but exclusive until all test executables
    // exit. Different compiler configurations have independent warm outputs.
    let lane_id = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\n{toolchain}",
            request.cache_identity.as_ref().unwrap_or(&root).display()
        ))
    );
    let lane = request.cache_directory.join(lane_id);
    fs::create_dir_all(&lane)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lane.join("lease.lock"))?;
    lock.lock_exclusive()?;
    let mut results = Vec::new();
    for check in &request.checks {
        let key = format!("{:x}", Sha256::digest(serde_json::to_vec(check)?));
        let receipt_path = lane.join(format!("{key}.json"));
        let before = fingerprint(&root, &meta, check, &toolchain)?;
        if !request.force_fresh {
            if let Ok(receipt) = fs::read(&receipt_path).and_then(|bytes| {
                serde_json::from_slice::<Receipt>(&bytes).map_err(std::io::Error::other)
            }) {
                if receipt.fingerprint == before
                    && receipt.result["success"] == true
                    && receipt.result["raw_log"]
                        .as_str()
                        .is_some_and(|p| Path::new(p).is_file())
                {
                    let mut result = receipt.result;
                    result["reused"] = json!(true);
                    results.push(result);
                    continue;
                }
            }
        }
        // Starting a fresh attempt retires older evidence, even if the attempt
        // fails or is cancelled without changing the declared source inputs.
        match fs::remove_file(&receipt_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let raw_path = lane.join(format!(
            "{key}-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let mut raw = fs::File::create(&raw_path)?;
        let mut args = check.args.clone();
        if args[0] == "test" && !args.iter().any(|arg| arg == "--no-fail-fast") {
            args.insert(1, "--no-fail-fast".into());
        }
        let separator = args.iter().position(|a| a == "--").unwrap_or(args.len());
        if !args.iter().any(|a| a.starts_with("--message-format")) {
            args.insert(separator, "--message-format=json".into());
        }
        if !args.iter().any(|a| a == "--locked" || a == "--frozen") {
            args.insert(1, "--locked".into());
        }
        let mut child = OwnedChild(
            crate::command("cargo")
                .args(&args)
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", lane.join("target"))
                .env("CARGO_BUILD_BUILD_DIR", lane.join("build"))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let stderr = child.0.stderr.take().context("stderr")?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(128);
        let stderr_sender = sender.clone();
        let err_reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if stderr_sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdout = child.0.stdout.take().context("stdout")?;
        let out_reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut text = String::new();
        let mut log_complete = true;
        for line in receiver {
            match line {
                Ok(line) => {
                    writeln!(raw, "{line}")?;
                    if text.len().saturating_add(line.len()) <= 64 * 1024 * 1024 {
                        text.push_str(&line);
                        text.push('\n');
                    } else {
                        log_complete = false;
                    }
                }
                Err(error) => {
                    log_complete = false;
                    writeln!(raw, "log reader error: {error}")?;
                }
            }
        }
        let status = child.0.wait()?;
        out_reader
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader panicked"))?;
        err_reader
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader panicked"))?;
        raw.sync_all()?;
        let after = fingerprint(&root, &meta, check, &toolchain)?;
        let mut inventory = crate::diagnostics::inventory(&text);
        inventory["failure_inventory_complete"] = json!(log_complete);
        let tests_executed = check.args[0] == "test"
            && inventory["test_summaries"].as_array().is_some_and(|s| {
                s.iter().any(|s| {
                    !s.as_str()
                        .unwrap_or_default()
                        .contains("0 passed; 0 failed")
                })
            });
        let success = status.success()
            && before == after
            && log_complete
            && (check.args[0] != "test" || tests_executed);
        let result = json!({"id":check.id,"args":args,"exit_code":status.code(),"success":success,
            "source_unchanged":before==after,"tests_executed":tests_executed,"reused":false,
            "dependency_fingerprint":after,"raw_log":raw_path,"inventory":inventory});
        if success {
            let receipt = Receipt {
                fingerprint: before,
                result: result.clone(),
            };
            let mut temp = tempfile::NamedTempFile::new_in(&lane)?;
            temp.write_all(&serde_json::to_vec(&receipt)?)?;
            temp.persist(&receipt_path).map_err(|e| e.error)?;
        }
        results.push(result);
    }
    Ok(
        json!({"success":results.iter().all(|r|r["success"]==true),"checks":results,"build_lane":lane}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_failure_invalidates_an_older_pass_with_the_same_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='external_input'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        let signal = dir.path().join("external-failure");
        fs::write(
            root.join("src/lib.rs"),
            format!(
                "#[test] fn external() {{ assert!(!std::path::Path::new({:?}).exists()); }}\n",
                signal.to_str().unwrap()
            ),
        )
        .unwrap();
        output("cargo", &["generate-lockfile", "--offline"], &root).unwrap();
        let execute = |force_fresh| {
            run(Request {
                repository: root.clone(),
                cache_directory: dir.path().join("cache"),
                cache_identity: None,
                action: Action::Run,
                checks: vec![Check {
                    id: "external".into(),
                    args: vec!["test".into(), "--lib".into()],
                }],
                allow_full_suite: false,
                force_fresh,
            })
            .unwrap()
        };
        assert_eq!(execute(false)["success"], true);
        fs::write(signal, "fail").unwrap();
        let fresh = execute(true);
        assert_eq!(fresh["success"], false);
        let retry = execute(false);
        assert_eq!(
            retry["success"], false,
            "a failed fresh run must retire the older pass: {retry}"
        );
        assert_eq!(retry["checks"][0]["reused"], false);
    }

    #[test]
    fn plan_from_workspace_subdirectory_includes_reverse_dependents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("rust");
        fs::create_dir_all(root.join("a/src")).unwrap();
        fs::create_dir_all(root.join("b/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=['a','b']\nresolver='2'\n",
        )
        .unwrap();
        fs::write(
            root.join("a/Cargo.toml"),
            "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(root.join("b/Cargo.toml"),"[package]\nname='b'\nversion='0.1.0'\nedition='2021'\n[dependencies]\na={path='../a'}\n").unwrap();
        for name in ["a", "b"] {
            fs::write(root.join(name).join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        }
        output("cargo", &["generate-lockfile", "--offline"], &root).unwrap();
        output("git", &["init", "--quiet"], dir.path()).unwrap();
        output("git", &["add", "."], dir.path()).unwrap();
        output(
            "git",
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            dir.path(),
        )
        .unwrap();
        fs::write(root.join("a/src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let result = run(Request {
            repository: root,
            cache_directory: dir.path().join("cache"),
            cache_identity: None,
            action: Action::Plan,
            checks: vec![],
            allow_full_suite: false,
            force_fresh: false,
        })
        .unwrap();
        assert_eq!(result["changed_paths"], json!(["a/src/lib.rs"]));
        let ids = result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["check:a", "check:b"]));
        assert!(
            !dir.path().join("cache").exists(),
            "planning must not execute validation"
        );
    }
    #[test]
    fn scoped_validation_rejects_broad_or_output_override_commands() {
        let check = |args: &[&str]| Check {
            id: "test".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        };
        assert!(validate_check(&check(&["test", "--locked"]), false).is_err());
        assert!(validate_check(&check(&["test", "--test", "focused"]), false).is_ok());
        assert!(validate_check(&check(&["test", "--workspace", "--lib"]), false).is_err());
        assert!(validate_check(&check(&["test", "--workspace"]), true).is_ok());
        assert!(validate_check(&check(&["check", "--target-dir=elsewhere"]), true).is_err());
        assert!(validate_check(&check(&["test", "--lib", "--", "--list"]), false).is_err());
        assert!(validate_check(&check(&["check", "--message-format=short"]), false).is_err());
    }
    #[test]
    fn dependency_tree_fingerprint_tracks_content_and_file_additions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("source.rs"), "first").unwrap();
        let hash = || {
            let mut h = Sha256::new();
            hash_tree(dir.path(), &mut h, &mut 0).unwrap();
            format!("{:x}", h.finalize())
        };
        let first = hash();
        fs::write(dir.path().join("source.rs"), "second").unwrap();
        let second = hash();
        assert_ne!(first, second);
        fs::write(dir.path().join("new.rs"), "new").unwrap();
        assert_ne!(second, hash());
        fs::create_dir(dir.path().join("target")).unwrap();
        let before = hash();
        fs::write(dir.path().join("target/cache"), "ignored build output").unwrap();
        assert_eq!(before, hash());
    }

    #[test]
    fn real_scoped_checks_reuse_passes_but_invalidate_dependencies_and_retain_failures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("a/src")).unwrap();
        fs::create_dir_all(root.join("b/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=['a','b']\nresolver='2'\n",
        )
        .unwrap();
        for name in ["a", "b"] {
            fs::write(
                root.join(name).join("Cargo.toml"),
                format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
            )
            .unwrap();
            fs::write(
                root.join(name).join("src/lib.rs"),
                "#[test] fn truth() { assert_eq!(2 + 2, 4); }\n",
            )
            .unwrap();
        }
        output("cargo", &["generate-lockfile", "--offline"], &root).unwrap();
        let run_check = || {
            run(Request {
                repository: root.clone(),
                cache_directory: dir.path().join("cache"),
                cache_identity: None,
                action: Action::Run,
                checks: vec![Check {
                    id: "a-unit".into(),
                    args: vec!["test".into(), "-p".into(), "a".into(), "--lib".into()],
                }],
                allow_full_suite: false,
                force_fresh: false,
            })
            .unwrap()
        };
        let first = run_check();
        assert_eq!(first["success"], true, "{first}");
        assert_eq!(first["checks"][0]["reused"], false);
        assert_eq!(first["checks"][0]["tests_executed"], true);
        fs::write(
            root.join("b/src/lib.rs"),
            "#[test] fn other() { assert!(true); }\n",
        )
        .unwrap();
        let second = run_check();
        assert_eq!(
            second["checks"][0]["reused"], true,
            "unrelated package must not invalidate a's receipt: {second}"
        );
        fs::write(
            root.join("a/src/lib.rs"),
            "#[test] fn truth() { assert_eq!(2 + 2, 5); }\n",
        )
        .unwrap();
        let third = run_check();
        assert_eq!(third["success"], false, "{third}");
        assert_eq!(third["checks"][0]["reused"], false);
        assert_eq!(
            third["checks"][0]["inventory"]["failed_tests"],
            json!(["truth"])
        );
        assert!(Path::new(third["checks"][0]["raw_log"].as_str().unwrap()).is_file());
        let fourth = run_check();
        assert_eq!(
            fourth["checks"][0]["reused"], false,
            "failures must not be reused as passes"
        );
    }
}
