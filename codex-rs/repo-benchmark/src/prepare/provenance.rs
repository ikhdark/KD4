use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
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
        use std::path::{Component, Prefix};
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

pub fn hash_tree(root: &Path) -> Result<String> {
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != "__pycache__")
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "symbolic link in frozen fixture: {}",
            entry.path().display()
        );
        if entry.file_type().is_file() {
            entries.push(entry.path().to_path_buf());
        }
    }
    entries.sort();
    let mut hash = Sha256::new();
    for path in entries {
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        hash.update(relative.as_bytes());
        hash.update([0]);
        hash.update(hash_file(&path)?.as_bytes());
        hash.update([0]);
    }
    Ok(format!("{:x}", hash.finalize()))
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

/// Deletes only the exact prepared workspace, never a source checkout or parent.
pub fn reset_workspace(prepared: &Path, workspace: &Path, snapshot: &Path) -> Result<()> {
    let prepared = fs::canonicalize(prepared)?;
    ensure!(
        workspace.file_name().is_some_and(|n| n == "workspace"),
        "reset target must be the prepared workspace"
    );
    ensure!(
        fs::canonicalize(workspace.parent().context("workspace parent")?)? == prepared,
        "reset target escaped preparation"
    );
    if workspace.exists() {
        ensure!(
            fs::canonicalize(workspace)? == prepared.join("workspace"),
            "workspace is a redirected path"
        );
        fs::remove_dir_all(workspace)?;
    }
    copy_tree(snapshot, workspace)?;
    git(workspace, &["init", "--quiet"])?;
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
    let digest = fs::read_to_string(path.with_extension("sha256"))?;
    ensure!(
        hash_bytes(&bytes) == digest.trim(),
        "changed JSON artifact: {}",
        path.display()
    );
    Ok(serde_json::from_slice(&bytes)?)
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
    use std::io::{BufRead, BufReader, Write};
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
