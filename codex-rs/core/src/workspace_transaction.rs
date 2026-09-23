//! Task-owned source snapshots and explicit reconciliation into a shared checkout.
//!
//! Nothing changes the original index, HEAD, or refs. Ignored build products are
//! deliberately not inputs. Links/submodules are rejected rather than silently
//! following dependencies outside the captured source boundary.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkspaceTransaction {
    pub(crate) origin: PathBuf,
    pub(crate) workdir: PathBuf,
    pub(crate) base: PathBuf,
    pub(crate) revision: String,
    pub(crate) files: BTreeMap<String, String>,
    pub(crate) reconciled: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReconcileResult {
    pub(crate) merged: bool,
    /// A merge receipt is not validation evidence for the combined source.
    pub(crate) validation_required: bool,
    pub(crate) conflicts: Vec<String>,
    pub(crate) changed_paths: Vec<String>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn git(root: &Path, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "-c",
            "core.hooksPath=NUL",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args);
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().context("run snapshot Git command")?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid snapshot path"
    );
    ensure!(
        !path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "Git metadata is not a source input"
    );
    Ok(path)
}

fn safe_path(root: &Path, name: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative(name)?.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "source link is not isolated: {}",
                    path.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "source reparse point is not isolated: {}",
                        path.display()
                    );
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(path)
}

fn bytes(root: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = safe_path(root, name)?;
    match fs::metadata(&path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && meta.len() <= MAX_FILE_BYTES,
                "unsupported source input: {}",
                path.display()
            );
            Ok(Some(fs::read(path)?))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn paths(root: &Path) -> Result<BTreeSet<String>> {
    let output = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths = BTreeSet::new();
    for path in output.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let name = std::str::from_utf8(path).context("non-UTF-8 source path")?;
        relative(name)?;
        paths.insert(name.to_owned());
        ensure!(
            paths.len() <= MAX_FILES,
            "snapshot exceeds {MAX_FILES} files"
        );
    }
    Ok(paths)
}

fn manifest(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut size = 0_u64;
    for name in paths(root)? {
        if let Some(contents) = bytes(root, &name)? {
            size += contents.len() as u64;
            ensure!(size <= MAX_SNAPSHOT_BYTES, "source snapshot exceeds 1 GiB");
            result.insert(name, digest(&contents));
        }
    }
    Ok(result)
}

fn copy_manifest(source: &Path, target: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    fs::create_dir_all(target)?;
    for (name, expected) in files {
        let contents = bytes(source, name)?.context("source disappeared during capture")?;
        ensure!(
            digest(&contents) == *expected,
            "source changed during capture: {name}"
        );
        let path = safe_path(target, name)?;
        fs::create_dir_all(path.parent().context("source parent")?)?;
        fs::write(&path, contents)?;
        fs::set_permissions(&path, fs::metadata(safe_path(source, name)?)?.permissions())?;
    }
    ensure!(
        manifest(source)? == *files,
        "source changed during capture; no usable snapshot was published"
    );
    Ok(())
}

fn init_snapshot(root: &Path) -> Result<()> {
    git(root, &["init", "--quiet"])?;
    git(root, &["add", "--all", "--force", "."])?;
    git(
        root,
        &[
            "-c",
            "user.name=Codex",
            "-c",
            "user.email=codex@localhost",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "--allow-empty",
            "-m",
            "Task input snapshot",
        ],
    )?;
    Ok(())
}

fn state_dir(home: &Path, thread: &str) -> Result<PathBuf> {
    ensure!(
        thread
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid task identity"
    );
    Ok(home.join("workspace-transactions").join(thread))
}

fn save(path: &Path, transaction: &WorkspaceTransaction) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("state parent")?)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(transaction)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|err| err.error)?;
    Ok(())
}

pub(crate) fn load(home: &Path, thread: &str) -> Result<Option<WorkspaceTransaction>> {
    let path = state_dir(home, thread)?.join("transaction.json");
    match fs::read(path) {
        Ok(contents) => {
            let transaction: WorkspaceTransaction = serde_json::from_slice(&contents)?;
            ensure!(
                transaction.workdir.starts_with(state_dir(home, thread)?),
                "invalid transaction location"
            );
            Ok(Some(transaction))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// An OS lock coordinates separate app-server processes, not just async tasks.
fn lock(home: &Path, root: &Path) -> Result<fs::File> {
    let dir = home.join("workspace-transactions").join("locks");
    fs::create_dir_all(&dir)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(digest(root.to_string_lossy().as_bytes())))?;
    file.lock()?;
    Ok(file)
}

pub(crate) fn begin(home: &Path, thread: &str, cwd: &Path) -> Result<WorkspaceTransaction> {
    let origin = codex_git_utils::get_git_repo_root(cwd)
        .context("workspace isolation requires a local Git checkout")?;
    let origin = fs::canonicalize(origin)?;
    let _lock = lock(home, &origin)?;
    if let Some(existing) = load(home, thread)? {
        ensure!(
            existing.origin == origin,
            "task already owns a transaction for another checkout"
        );
        if !existing.reconciled {
            return Ok(existing);
        }
    }
    let dir = state_dir(home, thread)?.join(uuid::Uuid::new_v4().to_string());
    let files = manifest(&origin)?;
    let base = dir.join("base");
    let workdir = dir.join("work");
    copy_manifest(&origin, &base, &files)?;
    copy_manifest(&origin, &workdir, &files)?;
    init_snapshot(&workdir)?;
    let transaction = WorkspaceTransaction {
        origin,
        workdir,
        base,
        revision: digest(&serde_json::to_vec(&files)?),
        files,
        reconciled: false,
    };
    save(
        &state_dir(home, thread)?.join("transaction.json"),
        &transaction,
    )?;
    Ok(transaction)
}

pub(crate) fn validation_snapshot(home: &Path, thread: &str) -> Result<WorkspaceTransaction> {
    let transaction =
        load(home, thread)?.context("begin a workspace transaction before validation")?;
    ensure!(
        !transaction.reconciled,
        "begin a new transaction before validation"
    );
    let dir = state_dir(home, thread)?
        .join("validation")
        .join(uuid::Uuid::new_v4().to_string());
    let files = manifest(&transaction.workdir)?;
    let workdir = dir.join("work");
    copy_manifest(&transaction.workdir, &workdir, &files)?;
    init_snapshot(&workdir)?;
    let snapshot = WorkspaceTransaction {
        origin: transaction.workdir,
        base: transaction.base,
        workdir,
        revision: digest(&serde_json::to_vec(&files)?),
        files,
        reconciled: false,
    };
    save(&dir.join("snapshot.json"), &snapshot)?;
    Ok(snapshot)
}

pub(crate) fn reconcile(home: &Path, thread: &str) -> Result<ReconcileResult> {
    let mut transaction = load(home, thread)?.context("no task workspace to reconcile")?;
    let _lock = lock(home, &transaction.origin)?;
    ensure!(!transaction.reconciled, "transaction already reconciled");
    let current = manifest(&transaction.workdir)?;
    let names: BTreeSet<_> = transaction
        .files
        .keys()
        .chain(current.keys())
        .cloned()
        .collect();
    let mut conflicts = Vec::new();
    let mut changes = Vec::new();
    for name in names {
        if transaction.files.get(&name) == current.get(&name) {
            continue;
        }
        let base = bytes(&transaction.base, &name)?;
        ensure!(
            base.as_ref().map(|b| digest(b)).as_ref() == transaction.files.get(&name),
            "base snapshot changed: {name}"
        );
        let ours = bytes(&transaction.workdir, &name)?;
        let theirs = bytes(&transaction.origin, &name)?;
        if ours == theirs {
            continue;
        }
        let merged = if theirs == base {
            Some(ours)
        } else if base.is_some() && ours.is_some() && theirs.is_some() {
            // Merge exactly the bytes whose identities were checked, not files
            // another process can rewrite while Git is reading them.
            let inputs = tempfile::tempdir()?;
            for (label, contents) in [("base", &base), ("ours", &ours), ("theirs", &theirs)] {
                fs::write(
                    inputs.path().join(label),
                    contents.as_deref().unwrap_or_default(),
                )?;
            }
            let mut command = Command::new("git");
            command
                .args(["merge-file", "--stdout", "--diff3"])
                .arg(inputs.path().join("theirs"))
                .arg(inputs.path().join("base"))
                .arg(inputs.path().join("ours"));
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let output = command.output()?;
            output.status.success().then_some(Some(output.stdout))
        } else {
            None
        };
        if let Some(merged) = merged {
            changes.push((name, theirs, merged));
        } else {
            conflicts.push(name);
        }
    }
    if !conflicts.is_empty() {
        return Ok(ReconcileResult {
            merged: false,
            validation_required: false,
            conflicts,
            changed_paths: Vec::new(),
        });
    }
    // Check all destinations before any write. Changes from actors outside the
    // cooperative lock cause a conflict, never an unconditional overwrite.
    for (name, expected, _) in &changes {
        ensure!(
            bytes(&transaction.origin, name)? == *expected,
            "destination changed before reconciliation: {name}"
        );
    }
    ensure!(
        manifest(&transaction.workdir)? == current,
        "task source changed during reconciliation"
    );
    // A retained journal makes partial I/O failure reviewable and recoverable.
    let journal = state_dir(home, thread)?.join("reconciliation.json");
    fs::write(&journal, serde_json::to_vec(&changes)?)?;
    let mut changed_paths = Vec::new();
    for (name, expected, replacement) in changes {
        ensure!(
            bytes(&transaction.origin, &name)? == expected,
            "destination changed during reconciliation: {name}; already published: {changed_paths:?}"
        );
        let path = safe_path(&transaction.origin, &name)?;
        match replacement {
            Some(contents) => {
                fs::create_dir_all(path.parent().context("destination parent")?)?;
                let mut temp =
                    tempfile::NamedTempFile::new_in(path.parent().context("destination parent")?)?;
                use std::io::Write;
                temp.write_all(&contents)?;
                let meta = fs::metadata(&path)
                    .or_else(|_| fs::metadata(transaction.workdir.join(&name)))?;
                temp.as_file().set_permissions(meta.permissions())?;
                temp.as_file().sync_all()?;
                temp.persist(&path).map_err(|err| err.error)?;
            }
            None => fs::remove_file(&path)?,
        }
        changed_paths.push(name);
    }
    transaction.reconciled = true;
    save(
        &state_dir(home, thread)?.join("transaction.json"),
        &transaction,
    )?;
    Ok(ReconcileResult {
        merged: true,
        validation_required: true,
        conflicts,
        changed_paths,
    })
}

/// Structured paths are remapped; executable shell text is never rewritten.
pub(crate) fn map_path(
    transaction: &WorkspaceTransaction,
    cwd: &Path,
    value: &str,
) -> Result<PathBuf> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let origin = dunce::simplified(&transaction.origin);
    let path = dunce::simplified(&path);
    if let Ok(tail) = path.strip_prefix(origin) {
        ensure!(
            !tail.components().any(|c| matches!(c, Component::ParentDir)),
            "path escapes transaction"
        );
        return Ok(transaction.workdir.join(tail));
    }
    Ok(path.to_path_buf())
}

pub(crate) fn evidence_cwd(
    turn: &crate::session::turn_context::TurnContext,
    cwd: &Path,
) -> Result<PathBuf> {
    if !matches!(
        turn.sandbox_policy(),
        codex_protocol::protocol::SandboxPolicy::DangerFullAccess
    ) {
        return Ok(cwd.to_path_buf());
    }
    let thread = turn.session_telemetry.conversation_id().to_string();
    match load(&turn.config.codex_home, &thread)?.filter(|tx| !tx.reconciled) {
        Some(tx) => map_path(&tx, cwd, "."),
        None => Ok(cwd.to_path_buf()),
    }
}

/// Runs before ordinary tool dispatch, so hooks and command admission observe
/// the actual isolated paths. Unknown/external tools are never rewritten.
pub(crate) fn route_call(
    home: &Path,
    thread: &str,
    cwd: &Path,
    name: &str,
    payload: &mut crate::tools::context::ToolPayload,
) -> Result<()> {
    use crate::tools::context::ToolPayload;
    if !matches!(
        name,
        "read_file"
            | "list_files"
            | "semantic_context"
            | "exec_command"
            | "shell_command"
            | "apply_patch"
    ) {
        return Ok(());
    }
    let Some(transaction) = load(home, thread)?.filter(|t| !t.reconciled) else {
        return Ok(());
    };
    match payload {
        ToolPayload::Function { arguments } => {
            let mut value: serde_json::Value = serde_json::from_str(arguments)?;
            ensure!(
                value.get("environment_id").is_none_or(|v| v.is_null()),
                "isolated workspaces use the primary local environment; omit environment_id"
            );
            if matches!(name, "read_file" | "list_files" | "semantic_context") {
                let field = if name == "semantic_context" {
                    "repository"
                } else {
                    "path"
                };
                let path = value
                    .get(field)
                    .and_then(|v| v.as_str())
                    .context("missing read path")?;
                if path.starts_with("skill:") {
                    return Ok(());
                }
                value[field] = serde_json::json!(map_path(&transaction, cwd, path)?);
            } else {
                let workdir = value.get("workdir").and_then(|v| v.as_str()).unwrap_or(".");
                let mapped = map_path(&transaction, cwd, workdir)?;
                let command = value
                    .get("cmd")
                    .or_else(|| value.get("command"))
                    .or_else(|| value.get("script_body"));
                let text = command.map(|v| v.to_string()).unwrap_or_else(|| {
                    serde_json::json!([value.get("program"), value.get("args")]).to_string()
                });
                let normalized = text.replace("\\\\", "\\").replace('\\', "/").to_lowercase();
                let origin = dunce::simplified(&transaction.origin)
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_lowercase();
                ensure!(
                    !normalized.contains(&origin),
                    "shell text refers to the live checkout; use task workspace paths: {}",
                    transaction.workdir.display()
                );
                let validation = if let Some(command) = command.and_then(|v| v.as_str()) {
                    matches!(
                        crate::validation_admission::classify_validation_script(command),
                        crate::validation_admission::ValidationClassification::Validation { .. }
                    )
                } else if value.get("program").is_some() {
                    let invocation =
                        crate::tools::handlers::command_shape::CommandInvocation::Argv {
                            program: value
                                .get("program")
                                .and_then(|v| v.as_str())
                                .context("missing program")?
                                .to_string(),
                            args: serde_json::from_value(
                                value
                                    .get("args")
                                    .cloned()
                                    .unwrap_or_else(|| serde_json::json!([])),
                            )?,
                        };
                    matches!(
                        crate::validation_admission::classify_validation(&invocation),
                        crate::validation_admission::ValidationClassification::Validation { .. }
                    )
                } else {
                    false
                };
                if validation {
                    ensure!(
                        name == "exec_command",
                        "use exec_command for validation in an isolated task"
                    );
                    ensure!(
                        !normalized.contains("--target-dir")
                            && !normalized.contains("cargo_target_dir")
                            && !normalized.contains("cargo_build_build_dir"),
                        "validation output paths are owned by the snapshot; remove target-directory overrides"
                    );
                    let work = dunce::simplified(&transaction.workdir);
                    let mapped = dunce::simplified(&mapped);
                    let tail = mapped
                        .strip_prefix(work)
                        .context("validation must run inside the task workspace")?;
                    let work_text = work.to_string_lossy().replace('\\', "/").to_lowercase();
                    ensure!(
                        !normalized.contains(&work_text),
                        "validation commands must use relative source paths so they execute on the captured revision"
                    );
                    let snapshot = validation_snapshot(home, thread)?;
                    value["workdir"] = serde_json::json!(snapshot.workdir.join(tail));
                    value["force_fresh"] = serde_json::json!(true);
                } else {
                    value["workdir"] = serde_json::json!(mapped);
                }
            }
            *arguments = serde_json::to_string(&value)?;
        }
        ToolPayload::Custom { input } if name == "apply_patch" => {
            let mut result = String::new();
            for line in input.lines() {
                let mut mapped = false;
                for prefix in [
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ] {
                    if let Some(path) = line.strip_prefix(prefix) {
                        let path = map_path(&transaction, cwd, path)?;
                        ensure!(
                            dunce::simplified(&path)
                                .starts_with(dunce::simplified(&transaction.workdir)),
                            "patch escapes task workspace"
                        );
                        result.push_str(prefix);
                        result.push_str(&path.to_string_lossy());
                        mapped = true;
                        break;
                    }
                }
                if !mapped {
                    result.push_str(line);
                }
                result.push('\n');
            }
            *input = result;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validation_context(
    home: &Path,
    thread: &str,
    cwd: &Path,
) -> Result<Option<WorkspaceTransaction>> {
    let root = state_dir(home, thread)?.join("validation");
    let cwd = dunce::simplified(cwd);
    if !cwd.starts_with(dunce::simplified(&root)) {
        return Ok(None);
    }
    for parent in cwd.ancestors() {
        if !parent.starts_with(dunce::simplified(&root)) {
            break;
        }
        let path = parent.join("snapshot.json");
        if path.is_file() {
            let snapshot: WorkspaceTransaction = serde_json::from_slice(&fs::read(path)?)?;
            ensure!(
                cwd.starts_with(dunce::simplified(&snapshot.workdir)),
                "validation cwd does not match its snapshot"
            );
            return Ok(Some(snapshot));
        }
    }
    anyhow::bail!("validation snapshot metadata is missing")
}

pub(crate) fn bind_validation_environment(
    snapshot: &WorkspaceTransaction,
    env: &mut std::collections::HashMap<String, String>,
) {
    let output_root = snapshot.workdir.with_file_name("artifacts");
    env.insert(
        "CARGO_TARGET_DIR".into(),
        output_root.join("target").to_string_lossy().into_owned(),
    );
    env.insert(
        "CARGO_BUILD_BUILD_DIR".into(),
        output_root.join("build").to_string_lossy().into_owned(),
    );
    env.insert(
        "CODEX_CARGO_LANE_TARGET_DIR".into(),
        output_root.join("target").to_string_lossy().into_owned(),
    );
    env.insert(
        "CODEX_VALIDATION_SOURCE_REVISION".into(),
        snapshot.revision.clone(),
    );
}

pub(crate) fn verify_validation_snapshot(snapshot: &WorkspaceTransaction) -> Result<()> {
    ensure!(
        manifest(&snapshot.workdir)? == snapshot.files,
        "validation source changed during execution; the result does not validate captured revision {}",
        snapshot.revision
    );
    Ok(())
}

#[cfg(test)]
#[path = "workspace_transaction_tests.rs"]
mod tests;
