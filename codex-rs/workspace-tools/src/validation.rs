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
    /// A captured source copy whose files must be marked newer than any build
    /// that already ran in the leased lane.
    #[serde(default)]
    pub snapshot_root: Option<PathBuf>,
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

fn hash_tree(root: &Path, hash: &mut Sha256) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
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
            hash_tree(&entry.path(), hash)?;
            hash.update(b"end-directory\0");
        } else if ty.is_file() {
            let file = fs::File::open(entry.path())?;
            let len = file.metadata()?.len();
            hash.update(len.to_le_bytes());
            // Bound memory, not input size: large dependency trees still need
            // complete content hashes before a passing receipt can be reused.
            let mut reader = BufReader::with_capacity(64 * 1024, file);
            let mut read_len = 0_u64;
            loop {
                let bytes = reader.fill_buf()?;
                if bytes.is_empty() {
                    break;
                }
                hash.update(bytes);
                let consumed = bytes.len();
                read_len += consumed as u64;
                reader.consume(consumed);
            }
            ensure!(
                read_len == len,
                "validation dependency changed size while fingerprinting: {}",
                entry.path().display()
            );
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
    hash.update(b"codex-validation-v3");
    hash.update(serde_json::to_vec(check)?);
    hash.update(toolchain.as_bytes());
    for package in meta["packages"].as_array().context("packages")? {
        if ids.contains(package["id"].as_str().unwrap_or_default()) {
            hash.update(package["name"].as_str().context("name")?.as_bytes());
            hash.update(package["version"].as_str().context("version")?.as_bytes());
            hash_tree(
                Path::new(package["manifest_path"].as_str().context("manifest path")?)
                    .parent()
                    .context("manifest parent")?,
                &mut hash,
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
                        | "CODEX_CARGO_LANE_TARGET_DIR"
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

const LANES: usize = 4;

fn open_lane(base: &Path, index: usize) -> anyhow::Result<(PathBuf, fs::File)> {
    let lane = if index == 0 {
        base.to_path_buf()
    } else {
        base.with_file_name(format!(
            "{}-{index}",
            base.file_name().context("lane name")?.to_string_lossy()
        ))
    };
    fs::create_dir_all(&lane)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lane.join("lease.lock"))?;
    Ok((lane, lock))
}

/// Leases the first idle output lane under `base` without waiting; `None`
/// when every lane is busy. The lease lasts until the returned file is dropped.
pub fn try_acquire_lane(base: &Path) -> anyhow::Result<Option<(PathBuf, fs::File, usize)>> {
    for index in 0..LANES {
        let (lane, lock) = open_lane(base, index)?;
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(Some((lane, lock, index))),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn acquire_lane(base: &Path) -> anyhow::Result<(PathBuf, fs::File, usize, u128)> {
    let started = std::time::Instant::now();
    if let Some((lane, lock, index)) = try_acquire_lane(base)? {
        return Ok((lane, lock, index, started.elapsed().as_millis()));
    }
    let (lane, lock) = open_lane(base, 0)?;
    lock.lock_exclusive()?;
    Ok((lane, lock, LANES, started.elapsed().as_millis()))
}

/// Marks every file of a captured source copy as modified now. Cargo judges
/// path-package freshness by mtime and hashes workspace members relative to
/// their root, so a copy captured before another snapshot's build started in
/// the same lane would otherwise reuse that build's artifacts. Call it only
/// while holding the lane's lease, after every earlier build there finished.
pub fn refresh_snapshot_mtimes(root: &Path) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push(entry.path());
                }
            } else if kind.is_file() {
                let mut options = fs::OpenOptions::new();
                // Timestamps need only attribute access; captured copies keep
                // their source permissions and may be read-only.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
                    options.access_mode(FILE_WRITE_ATTRIBUTES);
                }
                #[cfg(not(windows))]
                options.read(true);
                options
                    .open(entry.path())?
                    .set_modified(now)
                    .with_context(|| format!("refresh {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

pub fn run(request: Request) -> anyhow::Result<Value> {
    let run_started = std::time::Instant::now();
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
    // exit. Concurrent runs may lease one of the bounded overflow lanes.
    let lane_id = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\n{toolchain}",
            request.cache_identity.as_ref().unwrap_or(&root).display()
        ))
    );
    let (lane, _lease, busy_lanes, lease_wait_ms) =
        acquire_lane(&request.cache_directory.join(lane_id))?;
    if let Some(snapshot) = &request.snapshot_root {
        refresh_snapshot_mtimes(snapshot)?;
    }
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
                    result["executed_duration_ms"] = json!(0);
                    result["cargo_lock_messages"] = json!(0);
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
        let process_started = std::time::Instant::now();
        let mut child = OwnedChild(
            crate::command("cargo")
                .args(&args)
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", lane.join("target"))
                .env("CARGO_BUILD_BUILD_DIR", lane.join("build"))
                // Match the Cargo output override so nested repository runners
                // cannot redirect builds back into the caller's snapshot lane.
                .env("CODEX_CARGO_LANE_TARGET_DIR", lane.join("target"))
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
        let mut cargo_lock_messages = 0_u64;
        for line in receiver {
            match line {
                Ok(line) => {
                    if line.contains("Blocking waiting for file lock") {
                        cargo_lock_messages += 1;
                    }
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
        let executed_duration_ms = process_started.elapsed().as_millis();
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
            "dependency_fingerprint":after,"raw_log":raw_path,"inventory":inventory,
            "executed_duration_ms":executed_duration_ms,"cargo_lock_messages":cargo_lock_messages});
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
        json!({"success":results.iter().all(|r|r["success"]==true),"checks":results,"build_lane":lane,
            "diagnostics":{"busy_lanes":busy_lanes,"lease_wait_ms":lease_wait_ms,"total_duration_ms":run_started.elapsed().as_millis()}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_output_paths_reuse_passes_but_real_environment_changes_invalidate() {
        const CHILD_ROOT: &str = "CODEX_VALIDATION_SNAPSHOT_TEST_ROOT";
        if let Some(dir) = std::env::var_os(CHILD_ROOT) {
            let dir = PathBuf::from(dir);
            let revision = std::env::var("CODEX_VALIDATION_SOURCE_REVISION").unwrap();
            let result = run(Request {
                repository: dir.join(revision),
                cache_directory: dir.join("cache"),
                cache_identity: Some(dir.join("origin")),
                action: Action::Run,
                checks: vec![Check {
                    id: "snapshot".into(),
                    args: vec!["test".into(), "--lib".into()],
                }],
                allow_full_suite: false,
                force_fresh: false,
                snapshot_root: None,
            })
            .unwrap();
            fs::write(
                dir.join("result.json"),
                serde_json::to_vec(&result).unwrap(),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let first_root = dir.path().join("first");
        fs::create_dir_all(first_root.join("src")).unwrap();
        fs::write(
            first_root.join("Cargo.toml"),
            "[package]\nname='snapshot_environment'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            first_root.join("src/lib.rs"),
            format!(
                r#"#[test] fn environment() {{
                    std::fs::write({:?}, std::env::var("CODEX_CARGO_LANE_TARGET_DIR").unwrap()).unwrap();
                    assert_eq!(std::env::var("WORKSPACE_VALIDATION_TEST_INPUT").unwrap(), "pass");
                }}"#,
                dir.path().join("observed-lane.txt")
            ),
        )
        .unwrap();
        output("cargo", &["generate-lockfile", "--offline"], &first_root).unwrap();
        let second_root = dir.path().join("second");
        fs::create_dir_all(second_root.join("src")).unwrap();
        for name in ["Cargo.toml", "Cargo.lock", "src/lib.rs"] {
            fs::copy(first_root.join(name), second_root.join(name)).unwrap();
        }
        // Separate workers reproduce the environment bound by the core snapshot
        // launcher without mutating this test process's shared environment.
        let execute = |revision: &str, input: &str| {
            let artifacts = dir.path().join(revision).join("artifacts");
            let result = crate::command(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "validation::tests::snapshot_output_paths_reuse_passes_but_real_environment_changes_invalidate",
                    "--nocapture",
                ])
                .env(CHILD_ROOT, dir.path())
                .env("CODEX_VALIDATION_SOURCE_REVISION", revision)
                .env("CARGO_TARGET_DIR", artifacts.join("target"))
                .env("CARGO_BUILD_BUILD_DIR", artifacts.join("build"))
                .env("CODEX_CARGO_LANE_TARGET_DIR", artifacts.join("target"))
                .env("WORKSPACE_VALIDATION_TEST_INPUT", input)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "worker failed: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            serde_json::from_slice::<Value>(&fs::read(dir.path().join("result.json")).unwrap())
                .unwrap()
        };
        let first = execute("first", "pass");
        assert_eq!(first["success"], true, "{first}");
        assert_eq!(first["checks"][0]["reused"], false);
        let second = execute("second", "pass");
        assert_eq!(second["success"], true, "{second}");
        assert_eq!(second["checks"][0]["reused"], true, "{second}");
        assert_eq!(second["checks"][0]["executed_duration_ms"], 0);
        assert_eq!(
            second["checks"][0]["raw_log"],
            first["checks"][0]["raw_log"]
        );
        assert_eq!(
            PathBuf::from(fs::read_to_string(dir.path().join("observed-lane.txt")).unwrap()),
            Path::new(first["build_lane"].as_str().unwrap()).join("target")
        );
        let changed = execute("second", "fail");
        assert_eq!(changed["success"], false, "{changed}");
        assert_eq!(changed["checks"][0]["reused"], false);
        assert_ne!(
            changed["checks"][0]["dependency_fingerprint"],
            second["checks"][0]["dependency_fingerprint"]
        );
    }

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
                snapshot_root: None,
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
            snapshot_root: None,
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
    fn busy_validation_lane_uses_exclusive_overflow_and_returns_to_warm_primary() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("lane");
        let (first, first_lease, first_busy, _) = acquire_lane(&base).unwrap();
        assert_eq!(first, base);
        assert_eq!(first_busy, 0);
        let (second, second_lease, second_busy, _) = acquire_lane(&base).unwrap();
        assert_ne!(second, first);
        assert_eq!(second_busy, 1);
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(second.join("lease.lock"))
            .unwrap();
        assert_eq!(
            probe.try_lock_exclusive().unwrap_err().raw_os_error(),
            fs2::lock_contended_error().raw_os_error()
        );
        drop(first_lease);
        let (third, _third_lease, third_busy, _) = acquire_lane(&base).unwrap();
        assert_eq!(third, first);
        assert_eq!(third_busy, 0);
        drop(second_lease);
    }

    #[test]
    fn nonblocking_lane_lease_reports_saturation_and_reopens_released_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("lane");
        let mut leases = (0..LANES)
            .map(|expected| {
                let (lane, lease, index) = try_acquire_lane(&base).unwrap().expect("idle lane");
                assert_eq!(index, expected);
                (lane, lease)
            })
            .collect::<Vec<_>>();
        assert!(
            try_acquire_lane(&base).unwrap().is_none(),
            "a saturated origin must not wait for a lane"
        );
        let (released, lease) = leases.remove(2);
        drop(lease);
        let (lane, _lease, index) = try_acquire_lane(&base).unwrap().expect("released lane");
        assert_eq!((lane, index), (released, 2));
    }

    #[test]
    fn refreshed_snapshot_is_rebuilt_in_a_lane_built_from_another_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("lane");
        let snapshot = |name: &str, value: u32| {
            let root = dir.path().join(name).join("work");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='lane_probe'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
            )
            .unwrap();
            fs::write(
                root.join("src/lib.rs"),
                format!("#[test] fn probe() {{ println!(\"probe value {value}\"); }}\n"),
            )
            .unwrap();
            root
        };
        let run = |root: &Path| {
            let output = crate::command("cargo")
                .args(["test", "--offline", "--lib", "--", "--nocapture"])
                .current_dir(root)
                .env("CARGO_TARGET_DIR", lane.join("target"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        // The second snapshot was captured before the first build started in
        // the shared lane; Cargo hashes both copies identically.
        let second = snapshot("second", 2);
        let captured = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        for file in ["Cargo.toml", "src/lib.rs"] {
            fs::File::options()
                .write(true)
                .open(second.join(file))
                .unwrap()
                .set_modified(captured)
                .unwrap();
        }
        let first = snapshot("first", 1);
        assert!(run(&first).contains("probe value 1"));

        refresh_snapshot_mtimes(&second).unwrap();
        let output = run(&second);
        assert!(
            output.contains("probe value 2"),
            "the lane must not serve the other snapshot's build: {output}"
        );
    }

    #[test]
    fn snapshot_mtime_refresh_covers_read_only_sources_but_not_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let source = dir.path().join("src/nested/lib.rs");
        let read_only = dir.path().join("Cargo.toml");
        let metadata = dir.path().join(".git/index");
        for path in [&source, &read_only, &metadata] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "contents").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&read_only, permissions).unwrap();
        let before = std::time::SystemTime::now();

        refresh_snapshot_mtimes(dir.path()).unwrap();

        let modified = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
        assert!(modified(&source) >= before);
        assert!(modified(&read_only) >= before);
        assert!(fs::metadata(&read_only).unwrap().permissions().readonly());
        assert!(
            modified(&metadata) < before,
            "Git metadata is not a build input"
        );
        let mut permissions = fs::metadata(&read_only).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&read_only, permissions).unwrap();
    }
    #[test]
    fn dependency_tree_fingerprint_tracks_content_and_file_additions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("source.rs"), "first").unwrap();
        let hash = || {
            let mut h = Sha256::new();
            hash_tree(dir.path(), &mut h).unwrap();
            format!("{:x}", h.finalize())
        };
        let first = hash();
        let mut expected = Sha256::new();
        expected.update(b"source.rs\0");
        expected.update(5_u64.to_le_bytes());
        expected.update(b"first");
        assert_eq!(first, format!("{:x}", expected.finalize()));
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
    fn dependency_tree_fingerprint_includes_more_than_100_000_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut expected = Sha256::new();
        for directory in 0..100 {
            let name = format!("{directory:03}");
            let path = dir.path().join(&name);
            fs::create_dir(&path).unwrap();
            expected.update(name.as_bytes());
            expected.update(b"\0directory");
            for file in 0..1_000 {
                let name = format!("{file:03}");
                fs::write(path.join(&name), []).unwrap();
                expected.update(name.as_bytes());
                expected.update([0]);
                expected.update(0_u64.to_le_bytes());
            }
            expected.update(b"end-directory\0");
        }
        fs::write(dir.path().join("last"), b"included").unwrap();
        expected.update(b"last\0");
        expected.update(8_u64.to_le_bytes());
        expected.update(b"included");
        let mut actual = Sha256::new();
        hash_tree(dir.path(), &mut actual).unwrap();
        assert_eq!(actual.finalize(), expected.finalize());
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
        let large_input = root.join("a/large.bin");
        fs::File::create(&large_input)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
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
                snapshot_root: None,
            })
            .unwrap()
        };
        let first = run_check();
        assert_eq!(first["success"], true, "{first}");
        assert_eq!(first["checks"][0]["reused"], false);
        assert_eq!(first["checks"][0]["tests_executed"], true);
        assert!(first["checks"][0]["executed_duration_ms"].as_u64().unwrap() > 0);
        assert_eq!(first["diagnostics"]["busy_lanes"], 0);
        fs::write(
            root.join("b/src/lib.rs"),
            "#[test] fn other() { assert!(true); }\n",
        )
        .unwrap();
        let second = run_check();
        assert_eq!(second["checks"][0]["executed_duration_ms"], 0);
        assert_eq!(
            second["checks"][0]["reused"], true,
            "unrelated package must not invalidate a's receipt: {second}"
        );
        {
            use std::io::Seek;
            use std::io::SeekFrom;

            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&large_input)
                .unwrap();
            file.seek(SeekFrom::End(-1)).unwrap();
            file.write_all(b"x").unwrap();
        }
        let changed_input = run_check();
        assert_eq!(changed_input["success"], true, "{changed_input}");
        assert_eq!(changed_input["checks"][0]["reused"], false);
        assert_ne!(
            first["checks"][0]["dependency_fingerprint"],
            changed_input["checks"][0]["dependency_fingerprint"],
            "same-length changes beyond 64 MiB must invalidate cached passes"
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
