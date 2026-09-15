#[cfg(windows)]
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

use sha2::Digest;
use sha2::Sha256;

use crate::shell::ShellType;

use super::command_preflight::is_rg_program;
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

#[cfg(test)]
pub(crate) fn classify_rg_search_narrowing(
    command: &[String],
    shell_type: Option<ShellType>,
    cwd: &Path,
    repository_root: &Path,
) -> Result<Option<RgSearchNarrowing>, String> {
    classify_rg_search_with_repository(command, shell_type, cwd, || repository_root.to_path_buf())
        .map(|search| search.map(|(_, search)| search))
}

pub(crate) fn classify_rg_search_with_repository(
    command: &[String],
    shell_type: Option<ShellType>,
    cwd: &Path,
    repository_root: impl FnOnce() -> PathBuf,
) -> Result<Option<(PathBuf, RgSearchNarrowing)>, String> {
    let commands = match rg_argv_commands(command, shell_type) {
        Ok(commands) => commands,
        Err(_) => return Ok(None),
    };
    let rg_commands = commands
        .iter()
        .filter_map(|argv| {
            if !argv.first().is_some_and(|program| is_rg_program(program)) {
                return None;
            }
            let roles = RgArgumentRoles::parse(argv);
            roles.searches.then_some((argv, roles))
        })
        .collect::<Vec<_>>();
    if rg_commands.is_empty() {
        return Ok(None);
    }
    let repository_root = normalized_search_path(&repository_root());
    let mut query_identities = Vec::new();
    let mut search_identities = Vec::new();
    let mut all_targets = Vec::new();
    let mut explicit_input_files = Vec::new();
    let mut owner_scopes = Vec::new();
    let mut repository_wide = false;
    for (argv, roles) in &rg_commands {
        let path_indices = &roles.path_indices;
        let query_identity = rg_query_identity(argv, path_indices);
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
        explicit_input_files.extend(roles.input_files.iter().map(|value| {
            let path = Path::new(value);
            normalized_search_path(&cwd.join(path))
        }));
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
    let state_paths = search_state_paths(&all_targets, &explicit_input_files, &repository_root);
    Ok(Some((
        repository_root,
        RgSearchNarrowing {
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
                && !rg_commands.iter().any(|(_, roles)| roles.follows_links),
        },
    )))
}

// This optional cache must not turn a cheap search into an unbounded filesystem scan.
const SEARCH_SNAPSHOT_MAX_ENTRIES: usize = 512;
const SEARCH_SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(10);

struct SearchSnapshotBudget {
    remaining: usize,
    deadline: Instant,
    cancellation: CancellationToken,
}

impl SearchSnapshotBudget {
    fn check(&mut self) -> io::Result<()> {
        if self.remaining == 0
            || Instant::now() >= self.deadline
            || self.cancellation.is_cancelled()
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search snapshot budget exhausted",
            ));
        }
        self.remaining -= 1;
        Ok(())
    }
}

pub(crate) async fn observe_rg_search_scope_state(search: &mut RgSearchNarrowing) {
    observe_rg_search_scope_state_with(search, capture_search_scope_state).await;
}

async fn observe_rg_search_scope_state_with(
    search: &mut RgSearchNarrowing,
    mut capture: impl FnMut(&[PathBuf], &mut SearchSnapshotBudget) -> Option<String> + Send + 'static,
) {
    if !search.can_record_miss {
        return;
    }
    let state_paths = search.state_paths.clone();
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let mut budget = SearchSnapshotBudget {
        remaining: SEARCH_SNAPSHOT_MAX_ENTRIES,
        deadline: Instant::now() + SEARCH_SNAPSHOT_TIMEOUT,
        cancellation,
    };
    let observation = tokio::task::spawn_blocking(move || {
        let first = capture(&state_paths, &mut budget)?;
        // Each traversal must be able to inspect the same scope. Retain the
        // shared deadline and cancellation so the optional cache stays bounded.
        budget.remaining = SEARCH_SNAPSHOT_MAX_ENTRIES;
        let second = capture(&state_paths, &mut budget)?;
        (first == second).then_some(first)
    });
    // Dropping the wait cannot interrupt a filesystem call already in progress.
    // The drop guard cancels subsequent work, and late results are discarded.
    search.scope_state_identity = tokio::time::timeout(SEARCH_SNAPSHOT_TIMEOUT, observation)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
}

fn search_state_paths(
    targets: &[PathBuf],
    explicit_input_files: &[PathBuf],
    repository_root: &Path,
) -> Vec<PathBuf> {
    let mut paths = targets.to_vec();
    paths.extend(explicit_input_files.iter().cloned());
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

fn capture_search_scope_state(
    paths: &[PathBuf],
    budget: &mut SearchSnapshotBudget,
) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"kd4-rg-scope-state-v2\0");
    let mut covered_directories = Vec::new();
    let mut symlinks = Vec::new();
    for path in paths {
        // Coverage is established only by a completed traversal of a real directory.
        // Missing paths and symlinks must never cover their descendants.
        if covered_directories
            .iter()
            .any(|root: &PathBuf| path.starts_with(root))
            && !symlinks
                .iter()
                .any(|link: &PathBuf| path != link && path.starts_with(link))
        {
            continue;
        }
        if hash_scope_path(&mut hasher, path, budget, &mut symlinks).ok()? {
            covered_directories.push(path.clone());
        }
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn hash_scope_path(
    hasher: &mut Sha256,
    path: &Path,
    budget: &mut SearchSnapshotBudget,
    symlinks: &mut Vec<PathBuf>,
) -> io::Result<bool> {
    budget.check()?;
    hash_path_field(hasher, path);
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hasher.update(b"missing\0");
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        symlinks.push(path.to_path_buf());
        hasher.update(b"symlink\0");
        hash_path_field(hasher, &std::fs::read_link(path)?);
        return Ok(false);
    }
    if file_type.is_dir() {
        hasher.update(b"directory\0");
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            budget.check()?;
            entries.push(entry?.path());
        }
        entries.sort_unstable();
        for entry in entries {
            hash_scope_path(hasher, &entry, budget, symlinks)?;
        }
        return Ok(true);
    }
    if !file_type.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "search scope contains an unsupported filesystem object",
        ));
    }
    hasher.update(b"file\0");
    #[cfg(windows)]
    hash_trusted_file_token(hasher, &File::open(path)?, &metadata)?;
    #[cfg(not(windows))]
    hash_trusted_file_token(hasher, &metadata)?;
    Ok(false)
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
fn hash_trusted_file_token(hasher: &mut Sha256, metadata: &std::fs::Metadata) -> io::Result<()> {
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
fn hash_trusted_file_token(hasher: &mut Sha256, metadata: &std::fs::Metadata) -> io::Result<()> {
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

pub(crate) fn rg_search_path_operands(commands: &[Vec<String>]) -> Option<Vec<String>> {
    let mut saw_search = false;
    let mut operands = Vec::new();
    for argv in commands {
        if !argv.first().is_some_and(|program| is_rg_program(program)) {
            continue;
        }
        let roles = RgArgumentRoles::parse(argv);
        if !roles.searches {
            continue;
        }
        saw_search = true;
        operands.extend(
            roles
                .path_indices
                .into_iter()
                .filter_map(|index| argv.get(index).cloned()),
        );
    }
    saw_search.then_some(operands)
}

struct RgArgumentRoles<'a> {
    path_indices: Vec<usize>,
    input_files: Vec<&'a str>,
    searches: bool,
    follows_links: bool,
    files_mode: bool,
    explicit_pattern: bool,
}

impl<'a> RgArgumentRoles<'a> {
    fn option(&mut self, flag: &str, value: Option<&'a str>) {
        match flag {
            "--files" => self.files_mode = true,
            "-e" | "--regexp" => self.explicit_pattern = true,
            "-f" | "--file" | "--ignore-file" => {
                self.explicit_pattern |= flag != "--ignore-file";
                self.input_files.extend(value);
            }
            "-L" | "--follow" => self.follows_links = true,
            "-h" | "--help" | "-V" | "--version" | "--type-list" | "--pcre2-version"
            | "--generate" => self.searches = false,
            _ => {}
        }
    }

    fn parse(argv: &'a [String]) -> Self {
        let mut roles = Self {
            path_indices: Vec::new(),
            input_files: Vec::new(),
            searches: true,
            follows_links: false,
            files_mode: false,
            explicit_pattern: false,
        };
        let mut options_finished = false;
        let mut index = 1;
        while let Some(arg) = argv.get(index) {
            if options_finished || !arg.starts_with('-') || arg == "-" {
                roles.path_indices.push(index);
            } else if arg == "--" {
                options_finished = true;
            } else if arg.starts_with("--") {
                let (flag, mut value) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
                if value.is_none() && rg_option_consumes_next(flag) {
                    index += 1;
                    value = argv.get(index).map(String::as_str);
                }
                roles.option(flag, value);
            } else {
                // A value-taking short option consumes the rest of its cluster
                // or the next argument. Neither is another option.
                for (offset, flag) in arg.char_indices().skip(1) {
                    let flag_name = format!("-{flag}");
                    if rg_option_consumes_next(&flag_name) {
                        let remainder = &arg[offset + flag.len_utf8()..];
                        let value = if remainder.is_empty() {
                            index += 1;
                            argv.get(index).map(String::as_str)
                        } else {
                            Some(remainder)
                        };
                        roles.option(&flag_name, value);
                        break;
                    }
                    roles.option(&flag_name, None);
                }
            }
            index += 1;
        }
        if !roles.files_mode && !roles.explicit_pattern && !roles.path_indices.is_empty() {
            roles.path_indices.remove(0);
        }
        roles
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[tokio::test]
    async fn stable_scope_can_use_the_full_entry_budget_in_both_captures() {
        let command = vec!["rg".to_string(), "needle".to_string(), "src".to_string()];
        let mut search = classify_rg_search_narrowing(
            &command,
            None,
            Path::new("workspace"),
            Path::new("workspace"),
        )
        .unwrap()
        .unwrap();

        observe_rg_search_scope_state_with(&mut search, |_, budget| {
            for _ in 0..SEARCH_SNAPSHOT_MAX_ENTRIES {
                budget.check().ok()?;
            }
            Some("stable scope".to_string())
        })
        .await;

        assert_eq!(search.scope_state_identity.as_deref(), Some("stable scope"));
    }

    #[tokio::test]
    async fn changed_scope_is_not_reusable_between_captures() {
        let command = vec!["rg".to_string(), "needle".to_string(), "src".to_string()];
        let mut search = classify_rg_search_narrowing(
            &command,
            None,
            Path::new("workspace"),
            Path::new("workspace"),
        )
        .unwrap()
        .unwrap();
        let captures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_captures = std::sync::Arc::clone(&captures);
        observe_rg_search_scope_state_with(&mut search, move |_, budget| {
            budget.check().ok()?;
            let capture_number = captures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(format!("scope revision {capture_number}"))
        })
        .await;

        assert_eq!(
            observed_captures.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(search.scope_state_identity, None);
    }

    #[tokio::test]
    async fn slow_scope_observation_releases_the_request_and_discards_late_evidence() {
        let command = vec!["rg".to_string(), "needle".to_string(), "src".to_string()];
        let mut search = classify_rg_search_narrowing(
            &command,
            None,
            Path::new("workspace"),
            Path::new("workspace"),
        )
        .unwrap()
        .unwrap();
        search.scope_state_identity = Some("previous evidence".to_string());
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let mut finished = Some(finished);
        let observation = tokio::spawn(async move {
            observe_rg_search_scope_state_with(&mut search, move |_, budget| {
                if let Some(started) = started.take() {
                    let _ = started.send(());
                    blocked.recv().unwrap();
                    let _ = finished
                        .take()
                        .unwrap()
                        .send(budget.cancellation.is_cancelled());
                }
                Some("late evidence".to_string())
            })
            .await;
            search
        });
        ready.await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), observation).await;
        // Always release the worker before asserting, so a broken deadline does
        // not hang Tokio runtime shutdown.
        release.send(()).unwrap();
        let search = result
            .expect("optional observation must not wait on blocked I/O")
            .unwrap();
        assert_eq!(search.scope_state_identity, None);
        assert!(done.await.unwrap(), "late worker must observe cancellation");
    }
}
