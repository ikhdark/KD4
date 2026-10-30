use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

// Bound memory used while checking existing rules before an append.
const MAX_POLICY_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum AmendError {
    #[error("prefix rule requires at least one token")]
    EmptyPrefix,
    #[error("prefix rule tokens cannot be empty or whitespace-only")]
    EmptyPrefixToken,
    #[error("policy path has no parent: {path}")]
    MissingParent { path: PathBuf },
    #[error("failed to create policy directory {dir}: {source}")]
    CreatePolicyDir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to format prefix tokens: {source}")]
    SerializePrefix { source: serde_json::Error },
    #[error("failed to open policy file {path}: {source}")]
    OpenPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write to policy file {path}: {source}")]
    WritePolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to lock policy file {path}: {source}")]
    LockPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to seek policy file {path}: {source}")]
    SeekPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read policy file {path}: {source}")]
    ReadPolicyFile {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Note this thread uses advisory file locking and performs blocking I/O, so it should be used with
/// [`tokio::task::spawn_blocking`] when called from an async context.
pub fn blocking_append_allow_prefix_rule(
    policy_path: &Path,
    prefix: &[String],
) -> Result<(), AmendError> {
    if prefix.is_empty() {
        return Err(AmendError::EmptyPrefix);
    }
    if prefix[0].trim().is_empty() {
        return Err(AmendError::EmptyPrefixToken);
    }

    let tokens = prefix
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AmendError::SerializePrefix { source })?;
    let pattern = format!("[{}]", tokens.join(", "));
    let rule = format!(r#"prefix_rule(pattern={pattern}, decision="allow")"#);
    append_rule_line(policy_path, &rule)
}

fn append_rule_line(policy_path: &Path, rule: &str) -> Result<(), AmendError> {
    let dir = policy_path
        .parent()
        .ok_or_else(|| AmendError::MissingParent {
            path: policy_path.to_path_buf(),
        })?;
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(ref source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(AmendError::CreatePolicyDir {
                dir: dir.to_path_buf(),
                source,
            });
        }
    }

    append_locked_line(policy_path, rule)
}

fn append_locked_line(policy_path: &Path, line: &str) -> Result<(), AmendError> {
    // Create through any symlink before resolving the destination. Never replace
    // the symlink itself, including when it initially has a missing target.
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(policy_path)
        .map_err(|source| AmendError::OpenPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    let write_path =
        std::fs::canonicalize(policy_path).map_err(|source| AmendError::OpenPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    drop(file);

    // Lock a stable sibling: locking the destination inode cannot serialize
    // read/modify/replace operations once that inode has been replaced.
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(
        write_path
            .file_name()
            .ok_or_else(|| AmendError::MissingParent {
                path: policy_path.to_path_buf(),
            })?,
    );
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(write_path.with_file_name(lock_name))
        .and_then(|file| {
            file.lock()?;
            Ok(file)
        })
        .map_err(|source| AmendError::LockPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&write_path)
        .map_err(|source| AmendError::OpenPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;

    file.seek(SeekFrom::Start(0))
        .map_err(|source| AmendError::SeekPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    let mut contents = String::new();
    Read::by_ref(&mut file)
        .take(MAX_POLICY_FILE_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|source| AmendError::ReadPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 > MAX_POLICY_FILE_BYTES {
        return Err(AmendError::ReadPolicyFile {
            path: policy_path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "policy file exceeds the 64 MiB append limit",
            ),
        });
    }

    if contains_literal_rule(&contents, line) {
        return Ok(());
    }

    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(line);
    contents.push('\n');
    let permissions = file
        .metadata()
        .map_err(|source| AmendError::ReadPolicyFile {
            path: policy_path.to_path_buf(),
            source,
        })?
        .permissions();
    drop(file);

    // This crate is below codex-file-system in the dependency graph. Keep the
    // same temporary-file publication discipline without introducing a cycle.
    let publish = || -> std::io::Result<()> {
        let mut temporary =
            tempfile::NamedTempFile::new_in(write_path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "canonical policy path has no parent",
                )
            })?)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().set_permissions(permissions)?;
        temporary.as_file().sync_all()?;
        let (_file, path) = temporary.keep().map_err(|error| error.error)?;
        let path = tempfile::TempPath::try_from_path(path)?;
        std::fs::rename(&path, &write_path)?;
        Ok(())
    };
    publish().map_err(|source| AmendError::WritePolicyFile {
        path: policy_path.to_path_buf(),
        source,
    })?;
    drop(lock);

    Ok(())
}

// Only unconditional top-level statements can prove that an amendment exists.
// Parsing is needed only after the cheap textual candidate check succeeds.
fn contains_literal_rule(contents: &str, line: &str) -> bool {
    use starlark::syntax::AstModule;
    use starlark::syntax::Dialect;
    use starlark::syntax::ast::Stmt;
    if !contents.lines().any(|existing| existing == line) {
        return false;
    }
    let mut dialect = Dialect::Extended.clone();
    dialect.enable_f_strings = true;
    let Ok(ast) = AstModule::parse("amendment.rules", contents.to_string(), &dialect) else {
        return false;
    };
    let statements = match &ast.statement().node {
        Stmt::Statements(statements) => statements.as_slice(),
        _ => std::slice::from_ref(ast.statement()),
    };
    statements.iter().any(|statement| {
        matches!(&statement.node, Stmt::Expression(_))
            && ast.file_span(statement.span).source_span() == line
    })
}

