use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use crate::shell::ShellType;

use super::command_preflight::infer_direct_shell_type;
use super::command_preflight::matches_ignore_ascii_case;
use super::command_preflight::program_name;
use super::command_preflight::rg_argv_commands;
use super::command_preflight::rg_option_consumes_next;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub(crate) enum RgSearchBreadth {
    Narrow,
    Broad,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RgSearchNarrowing {
    pub(crate) breadth: RgSearchBreadth,
    pub(crate) query_identity: String,
    pub(crate) search_identity: String,
    pub(crate) scope_identity: String,
    pub(crate) parent_scope_identity: Option<String>,
    pub(crate) scope_state_identity: Option<String>,
    pub(crate) state_paths: Vec<PathBuf>,
    pub(crate) can_record_miss: bool,
}

pub(crate) fn classify_rg_search_narrowing(
    command: &[String],
    shell_type: Option<ShellType>,
    cwd: &Path,
    repository_root: &Path,
) -> Result<Option<RgSearchNarrowing>, String> {
    let commands = match rg_argv_commands(command, shell_type) {
        Ok(commands) => commands,
        Err(_) => return Ok(None),
    };
    let rg_commands = commands
        .iter()
        .filter(|argv| {
            argv.first()
                .map(|program| program_name(program))
                .is_some_and(|program| matches_ignore_ascii_case(program, &["rg", "rga"]))
                && rg_invocation_searches(argv)
        })
        .collect::<Vec<_>>();
    if rg_commands.is_empty() {
        return Ok(None);
    }
    let repository_root = normalized_search_path(repository_root);
    let mut query_identities = Vec::new();
    let mut search_identities = Vec::new();
    let mut all_targets = Vec::new();
    let mut explicit_ignore_files = Vec::new();
    let mut owner_scopes = Vec::new();
    let mut repository_wide = false;
    for argv in &rg_commands {
        let path_indices = rg_path_operand_indices(argv);
        let query_identity = rg_query_identity(argv, &path_indices);
        let mut targets = if path_indices.is_empty() {
            vec![normalized_search_path(cwd)]
        } else {
            path_indices
                .iter()
                .map(|index| {
                    let path = Path::new(&argv[*index]);
                    let target = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        cwd.join(path)
                    };
                    normalized_search_path(&target)
                })
                .collect::<Vec<_>>()
        };
        targets.sort_unstable();
        targets.dedup();
        for target in &targets {
            match repository_owner_scope(target, &repository_root) {
                Some(owner_scope) => owner_scopes.push(owner_scope),
                None => repository_wide = true,
            }
        }
        all_targets.extend(targets.iter().cloned());
        explicit_ignore_files.extend(rg_ignore_file_paths(argv, cwd));
        search_identities.push(format!(
            "{}\u{1d}{query_identity}\u{1d}{}",
            program_name(&argv[0]),
            targets
                .iter()
                .map(|target| target.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\u{1c}")
        ));
        query_identities.push(query_identity);
    }
    all_targets.sort_unstable();
    all_targets.dedup();
    owner_scopes.sort_unstable();
    owner_scopes.dedup();
    repository_wide |= owner_scopes.len() > 1;
    let scope_identity = path_scope_identity(&all_targets);
    let parent_scope_identity = parent_scope_identity(&all_targets, &repository_root);
    let state_paths = search_state_paths(&all_targets, &explicit_ignore_files, &repository_root);
    Ok(Some(RgSearchNarrowing {
        breadth: if repository_wide {
            RgSearchBreadth::Broad
        } else {
            RgSearchBreadth::Narrow
        },
        query_identity: query_identities.join("\u{1f}"),
        search_identity: search_identities.join("\u{1f}"),
        scope_identity,
        parent_scope_identity,
        scope_state_identity: None,
        state_paths,
        // A compound process exit code cannot be attributed to its rg command.
        // Following symlinks would require snapshotting content outside the
        // classified target paths, so those searches remain retryable.
        can_record_miss: commands.len() == 1
            && rg_commands.len() == 1
            && !rg_commands.iter().any(|argv| rg_follows_links(argv)),
    }))
}

pub(crate) async fn observe_rg_search_scope_state(search: &mut RgSearchNarrowing) {
    if !search.can_record_miss {
        return;
    }
    let state_paths = search.state_paths.clone();
    search.scope_state_identity = tokio::task::spawn_blocking(move || {
        let first = capture_search_scope_state(&state_paths)?;
        let second = capture_search_scope_state(&state_paths)?;
        (first == second).then_some(first)
    })
    .await
    .ok()
    .flatten();
}

fn search_state_paths(
    targets: &[PathBuf],
    explicit_ignore_files: &[PathBuf],
    repository_root: &Path,
) -> Vec<PathBuf> {
    let mut paths = targets.to_vec();
    paths.extend(explicit_ignore_files.iter().cloned());
    paths.push(repository_root.join(".git").join("info").join("exclude"));
    for target in targets {
        let mut ancestor = if target.is_dir() {
            Some(target.as_path())
        } else {
            target.parent()
        };
        while let Some(directory) = ancestor {
            if !directory.starts_with(repository_root) {
                break;
            }
            for ignore_name in [".gitignore", ".ignore", ".rgignore"] {
                paths.push(directory.join(ignore_name));
            }
            if directory == repository_root {
                break;
            }
            ancestor = directory.parent();
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn rg_ignore_file_paths(argv: &[String], cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut index = 1usize;
    while let Some(argument) = argv.get(index) {
        let value = if argument == "--ignore-file" {
            index = index.saturating_add(1);
            argv.get(index).map(String::as_str)
        } else {
            argument.strip_prefix("--ignore-file=")
        };
        if let Some(value) = value {
            let path = Path::new(value);
            let normalized_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            paths.push(normalized_search_path(&normalized_path));
        }
        index = index.saturating_add(1);
    }
    paths
}

fn rg_follows_links(argv: &[String]) -> bool {
    argv.iter().skip(1).any(|argument| {
        argument == "--follow"
            || argument == "-L"
            || (argument.starts_with('-')
                && !argument.starts_with("--")
                && argument[1..].contains('L'))
    })
}

fn capture_search_scope_state(paths: &[PathBuf]) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"kd4-rg-scope-state-v1\0");
    for path in paths {
        hash_scope_path(&mut hasher, path).ok()?;
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn hash_scope_path(hasher: &mut Sha256, path: &Path) -> io::Result<()> {
    hash_path_field(hasher, path);
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hasher.update(b"missing\0");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        hasher.update(b"symlink\0");
        hash_path_field(hasher, &std::fs::read_link(path)?);
        return Ok(());
    }
    if file_type.is_dir() {
        hasher.update(b"directory\0");
        let mut entries = std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort_unstable();
        for entry in entries {
            hash_scope_path(hasher, &entry)?;
        }
        return Ok(());
    }
    if !file_type.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "search scope contains an unsupported filesystem object",
        ));
    }
    hasher.update(b"file\0");
    hash_trusted_file_token(hasher, &File::open(path)?, &metadata)
}

#[cfg(windows)]
fn hash_path_field(hasher: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let encoded = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    hasher.update((encoded.len() as u64).to_le_bytes());
    hasher.update(encoded);
}

#[cfg(unix)]
fn hash_path_field(hasher: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let encoded = path.as_os_str().as_bytes();
    hasher.update((encoded.len() as u64).to_le_bytes());
    hasher.update(encoded);
}

#[cfg(not(any(unix, windows)))]
fn hash_path_field(hasher: &mut Sha256, path: &Path) {
    let encoded = path.to_string_lossy();
    hasher.update((encoded.len() as u64).to_le_bytes());
    hasher.update(encoded.as_bytes());
}

#[cfg(windows)]
fn hash_trusted_file_token(
    hasher: &mut Sha256,
    file: &File,
    metadata: &std::fs::Metadata,
) -> io::Result<()> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileBasicInfo;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let handle = file.as_raw_handle() as HANDLE;
    let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: `file` owns a valid handle and `id` is correctly sized writable storage.
    let id_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            id.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if id_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut basic = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: `file` owns a valid handle and `basic` is correctly sized writable storage.
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            basic.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if basic_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful calls initialized both complete structures.
    let id = unsafe { id.assume_init() };
    let basic = unsafe { basic.assume_init() };
    if id.FileId.Identifier.iter().all(|byte| *byte == 0)
        || basic.LastWriteTime == 0
        || basic.ChangeTime == 0
    {
        return Err(io::Error::other(
            "search scope file does not expose a stable metadata token",
        ));
    }
    hasher.update(metadata.len().to_le_bytes());
    hasher.update(id.VolumeSerialNumber.to_le_bytes());
    hasher.update(id.FileId.Identifier);
    hasher.update(basic.FileAttributes.to_le_bytes());
    hasher.update(basic.LastWriteTime.to_le_bytes());
    hasher.update(basic.ChangeTime.to_le_bytes());
    Ok(())
}

#[cfg(unix)]
fn hash_trusted_file_token(
    hasher: &mut Sha256,
    _file: &File,
    metadata: &std::fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    hasher.update(metadata.len().to_le_bytes());
    hasher.update(metadata.dev().to_le_bytes());
    hasher.update(metadata.ino().to_le_bytes());
    hasher.update(metadata.mode().to_le_bytes());
    hasher.update(metadata.mtime().to_le_bytes());
    hasher.update(metadata.mtime_nsec().to_le_bytes());
    hasher.update(metadata.ctime().to_le_bytes());
    hasher.update(metadata.ctime_nsec().to_le_bytes());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn hash_trusted_file_token(
    hasher: &mut Sha256,
    _file: &File,
    metadata: &std::fs::Metadata,
) -> io::Result<()> {
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            io::Error::other(format!(
                "search scope timestamp predates the Unix epoch: {error}"
            ))
        })?;
    hasher.update(metadata.len().to_le_bytes());
    hasher.update(modified.as_nanos().to_le_bytes());
    Ok(())
}

pub(crate) fn classify_rg_search_narrowing_without_native_scope(
    _command: &[String],
    _shell_type: Option<ShellType>,
) -> Option<RgSearchNarrowing> {
    // Search narrowing is a token-efficiency optimization, not an admission boundary. Foreign
    // workdirs cannot be mapped to native paths, so leave the command unchanged and let the
    // ordinary sandbox and permission checks decide whether it may run.
    None
}

fn rg_invocation_searches(argv: &[String]) -> bool {
    !argv.iter().skip(1).any(|arg| {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        matches!(
            flag,
            "-h" | "--help" | "-V" | "--version" | "--type-list" | "--pcre2-version" | "--generate"
        )
    })
}

fn rg_query_identity(argv: &[String], path_indices: &[usize]) -> String {
    argv.iter()
        .enumerate()
        .filter(|(index, _)| *index != 0 && !path_indices.contains(index))
        .map(|(_, argument)| argument.as_str())
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

fn repository_owner_scope(target: &Path, repository_root: &Path) -> Option<PathBuf> {
    let relative = target.strip_prefix(repository_root).ok()?;
    let mut components = relative.components();
    let first = components.next()?.as_os_str();
    if first.eq_ignore_ascii_case("codex-rs") {
        let second = components.next()?.as_os_str();
        Some(repository_root.join(first).join(second))
    } else {
        Some(repository_root.join(first))
    }
}

fn path_scope_identity(targets: &[PathBuf]) -> String {
    targets
        .iter()
        .map(|target| target.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\u{1c}")
}

fn parent_scope_identity(targets: &[PathBuf], repository_root: &Path) -> Option<String> {
    let mut parents = Vec::new();
    for target in targets {
        if target == repository_root || !target.starts_with(repository_root) {
            return None;
        }
        parents.push(target.parent()?.to_path_buf());
    }
    parents.sort_unstable();
    parents.dedup();
    Some(path_scope_identity(&parents))
}

fn normalized_search_path(path: &Path) -> PathBuf {
    #[cfg(test)]
    SEARCH_PATH_NORMALIZATION_COUNT.with(|count| count.set(count.get() + 1));
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                component => normalized.push(component.as_os_str()),
            }
        }
        normalized
    })
}

#[cfg(test)]
thread_local! {
    static SEARCH_PATH_NORMALIZATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_search_path_normalization_count() {
    SEARCH_PATH_NORMALIZATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn search_path_normalization_count() -> usize {
    SEARCH_PATH_NORMALIZATION_COUNT.with(std::cell::Cell::get)
}

pub(crate) fn rg_search_path_operands(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Option<Vec<String>>, String> {
    let commands = rg_argv_commands(
        command,
        shell_type.or_else(|| infer_direct_shell_type(command)),
    )?;
    let mut saw_search = false;
    let mut operands = Vec::new();
    for argv in commands {
        let is_rg_search = argv
            .first()
            .map(|program| program_name(program))
            .is_some_and(|program| matches_ignore_ascii_case(program, &["rg", "rga", "ripgrep"]))
            && rg_invocation_searches(&argv);
        if !is_rg_search {
            continue;
        }
        saw_search = true;
        operands.extend(
            rg_path_operand_indices(&argv)
                .into_iter()
                .filter_map(|index| argv.get(index).cloned()),
        );
    }
    Ok(saw_search.then_some(operands))
}

fn rg_path_operand_indices(argv: &[String]) -> Vec<usize> {
    let files_mode = argv.iter().skip(1).any(|arg| arg == "--files");
    let explicit_pattern = argv.iter().skip(1).any(|arg| {
        matches!(arg.as_str(), "-e" | "--regexp" | "-f" | "--file")
            || (arg.starts_with("-e") && arg.len() > 2)
            || arg.starts_with("--regexp=")
            || (arg.starts_with("-f") && arg.len() > 2)
            || arg.starts_with("--file=")
    });
    let mut positional = Vec::new();
    let mut options_finished = false;
    let mut index = 1usize;
    while let Some(arg) = argv.get(index) {
        if !options_finished && arg == "--" {
            options_finished = true;
            index += 1;
            continue;
        }
        if !options_finished && rg_option_consumes_next(arg) {
            index += 2;
            continue;
        }
        if !options_finished && arg.starts_with('-') {
            index += 1;
            continue;
        }
        positional.push(index);
        index += 1;
    }
    if files_mode || explicit_pattern {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    }
}
