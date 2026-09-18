use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentity {
    pub path: PathBuf,
    pub sha256: String,
}

impl FileIdentity {
    pub fn record(path: &Path) -> Result<Self> {
        Ok(Self {
            path: fs::canonicalize(path)?,
            sha256: hash_file(path)?,
        })
    }
    pub fn verify(&self) -> Result<()> {
        ensure!(
            hash_file(&self.path)? == self.sha256,
            "changed prepared artifact: {}",
            self.path.display()
        );
        Ok(())
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn command_output(command: &mut Command) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().context("execute preparation command")?;
    ensure!(
        output.status.success(),
        "command {:?} failed: {}",
        command.get_program(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(command_output(
        Command::new("git")
            .args(["-c", "core.longpaths=true"])
            .arg("-C")
            .arg(root)
            .args(args),
    )?)?
    .trim()
    .to_owned())
}

/// Git's worktree path parser rejects Windows verbatim prefixes. This changes
/// only the spelling of the absolute path, never its resolved destination.
pub fn git_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::Component;
        use std::path::Prefix;
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            let base = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => Some(PathBuf::from(format!("{}:\\", drive as char))),
                Prefix::VerbatimUNC(server, share) => {
                    Some(PathBuf::from(r"\\").join(server).join(share))
                }
                _ => None,
            };
            if let Some(base) = base {
                return base.join(path.components().skip(2).collect::<PathBuf>());
            }
        }
    }
    path.to_path_buf()
}

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let root = PathBuf::from(git(start, &["rev-parse", "--show-toplevel"])?);
    ensure!(
        root.join("codex-rs/Cargo.toml").is_file(),
        "repository has no codex-rs workspace"
    );
    Ok(fs::canonicalize(root)?)
}

pub struct TreeInventory {
    pub sha256: String,
    pub files: BTreeMap<PathBuf, String>,
    directories: BTreeSet<PathBuf>,
    permissions: BTreeMap<PathBuf, fs::Permissions>,
}

fn reject_redirect(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    let redirected = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        redirected || metadata.file_attributes() & 0x400 != 0
    };
    ensure!(!redirected, "redirected fixture path: {}", path.display());
    Ok(())
}

fn tree_inventory(root: &Path, excluded: &[&str]) -> Result<TreeInventory> {
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut permissions = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || !excluded.contains(&e.file_name().to_string_lossy().as_ref())
        })
    {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        reject_redirect(entry.path(), &metadata)?;
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        if metadata.is_file() {
            files.insert(relative.clone(), hash_file(entry.path())?);
            permissions.insert(relative, metadata.permissions());
        } else {
            ensure!(
                metadata.is_dir(),
                "unsupported fixture entry: {}",
                entry.path().display()
            );
            directories.insert(relative);
        }
    }
    let mut hash = Sha256::new();
    for (relative, digest) in &files {
        hash.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update([0]);
        hash.update(digest.as_bytes());
        hash.update([0]);
    }
    Ok(TreeInventory {
        sha256: format!("{:x}", hash.finalize()),
        files,
        directories,
        permissions,
    })
}

pub fn hash_tree(root: &Path) -> Result<String> {
    Ok(tree_inventory(root, &[".git", "__pycache__"])?.sha256)
}

/// Hash source evidence once, excluding generated dependencies and runtime caches.
pub fn source_tree_inventory(root: &Path) -> Result<TreeInventory> {
    tree_inventory(root, &[".git", "target", "node_modules", "__pycache__"])
}

pub fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != "__pycache__")
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "unsupported fixture link: {}",
            entry.path().display()
        );
        let dest = target.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(dest)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Restore only the exact owned workspace. Unchanged bytes stay in place.
pub fn reset_workspace(
    prepared: &Path,
    workspace: &Path,
    snapshot: &Path,
    expected_sha256: &str,
) -> Result<()> {
    let prepared = fs::canonicalize(prepared)?;
    ensure!(
        workspace.file_name().is_some_and(|n| n == "workspace"),
        "reset target must be the prepared workspace"
    );
    ensure!(
        fs::canonicalize(workspace.parent().context("workspace parent")?)? == prepared,
        "reset target escaped preparation"
    );
    if let Ok(metadata) = fs::symlink_metadata(workspace) {
        reject_redirect(workspace, &metadata)?;
        ensure!(
            fs::canonicalize(workspace)? == prepared.join("workspace"),
            "workspace is a redirected path"
        );
    }
    let snapshot_root = fs::canonicalize(snapshot)?;
    let target_root = prepared.join("workspace");
    ensure!(
        !snapshot_root.starts_with(&target_root) && !target_root.starts_with(&snapshot_root),
        "snapshot and workspace must be disjoint"
    );
    // Complete integrity checking before touching the previous attempt's state.
    let inventory = tree_inventory(snapshot, &[".git", "__pycache__"])?;
    ensure!(
        inventory.sha256 == expected_sha256,
        "changed fixture snapshot: {}",
        snapshot.display()
    );
    let mut existing = Vec::new();
    if workspace.exists() {
        for entry in walkdir::WalkDir::new(workspace).follow_links(false) {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            reject_redirect(entry.path(), &metadata)?;
            ensure!(
                metadata.is_file() || metadata.is_dir(),
                "unsupported workspace entry"
            );
            if entry.depth() > 0 {
                existing.push((
                    entry.path().strip_prefix(workspace)?.to_path_buf(),
                    metadata.is_dir(),
                ));
            }
        }
    }
    // Children are removed first, including stale caches and all old Git state.
    existing.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (relative, directory) in existing {
        let keep = if directory {
            inventory.directories.contains(&relative)
        } else {
            inventory.files.contains_key(&relative)
        };
        if !keep {
            let path = workspace.join(relative);
            if directory {
                fs::remove_dir(path)?;
            } else {
                make_writable(&path)?;
                fs::remove_file(path)?;
            }
        }
    }
    fs::create_dir_all(workspace)?;
    for relative in &inventory.directories {
        fs::create_dir_all(workspace.join(relative))?;
    }
    for (relative, expected) in &inventory.files {
        let destination = workspace.join(relative);
        let unchanged = destination.is_file() && hash_file(&destination)? == *expected;
        if !unchanged {
            if destination.is_file() {
                make_writable(&destination)?;
            }
            fs::copy(snapshot.join(relative), &destination)?;
        }
        // Content equality does not establish executable-bit equality.
        fs::set_permissions(&destination, inventory.permissions[relative].clone())?;
    }
    git(workspace, &["init", "--quiet"])?;
    Ok(())
}

fn make_writable(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        if permissions.readonly() {
            // Windows-only: this clears the read-only attribute and grants no
            // extra access. The lint's concern is the Unix world-writable mode.
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "cfg(windows) only, where this clears the read-only attribute"
            )]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, &bytes)?;
    fs::write(path.with_extension("sha256"), hash_bytes(&bytes))?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(digest) = value
        .as_object_mut()
        .and_then(|v| v.remove("_recordSha256"))
    {
        ensure!(
            digest.as_str() == Some(hash_bytes(&serde_json::to_vec(&value)?).as_str()),
            "changed JSON artifact: {}",
            path.display()
        );
        return Ok(serde_json::from_value(value)?);
    }
    let digest = fs::read_to_string(path.with_extension("sha256"))?;
    ensure!(
        hash_bytes(&bytes) == digest.trim(),
        "changed JSON artifact: {}",
        path.display()
    );
    Ok(serde_json::from_slice(&bytes)?)
}

/// Mutable checkpoints publish their payload and checksum in one atomic rename.
/// A crash before publication leaves the previous complete record readable.
pub fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut value = serde_json::to_value(value)?;
    let digest = hash_bytes(&serde_json::to_vec(&value)?);
    let object = value
        .as_object_mut()
        .context("atomic record must be an object")?;
    ensure!(
        !object.contains_key("_recordSha256"),
        "reserved record checksum key"
    );
    object.insert("_recordSha256".into(), digest.into());
    let bytes = serde_json::to_vec_pretty(&value)?;
    let temporary = path.with_extension(format!("{}.pending", super::unique_id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path).context("atomically publish checkpoint")
    })();
    if result.is_err() {
        // The closure releases the file even on write/sync failure, so Windows
        // can remove the unpublished record without hiding the original error.
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Materialize exactly the named commit's Git blobs, never the working tree or index.
pub fn materialize_commit(repo: &Path, revision: &str, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let tree = command_output(Command::new("git").arg("-C").arg(repo).args([
        "ls-tree",
        "-rz",
        "--full-tree",
        revision,
    ]))?;
    // One cat-file process avoids a process launch for every tracked file.
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::process::Stdio;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn()?;
    let result = (|| -> Result<()> {
        let mut stdin = child.stdin.take().context("cat-file stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("cat-file stdout")?);
        for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let (header, path) = entry.split_at(
                entry
                    .iter()
                    .position(|b| *b == b'\t')
                    .context("Git tree record")?,
            );
            let header = std::str::from_utf8(header)?;
            let parts: Vec<_> = header.split_whitespace().collect();
            ensure!(
                parts.len() == 3 && parts[1] == "blob" && parts[0] != "120000",
                "unsupported Git tree entry: {header}"
            );
            let relative = Path::new(std::str::from_utf8(&path[1..])?);
            ensure!(
                relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
                "invalid Git tree path"
            );
            writeln!(stdin, "{}", parts[2])?;
            stdin.flush()?;
            let mut response = String::new();
            stdout.read_line(&mut response)?;
            let size: usize = response
                .split_whitespace()
                .nth(2)
                .context("cat-file size")?
                .parse()?;
            let mut data = vec![0; size];
            stdout.read_exact(&mut data)?;
            let mut newline = [0];
            stdout.read_exact(&mut newline)?;
            let target = destination.join(relative);
            fs::create_dir_all(target.parent().context("blob parent")?)?;
            fs::write(&target, data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if parts[0] == "100755" {
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                }
            }
        }
        drop(stdin);
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    ensure!(status.success(), "Git blob materialization failed");
    Ok(())
}
