#[cfg(windows)]
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Semaphore;
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
            repository_wide |= target == &repository_root || !target.starts_with(&repository_root);
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
                && rg_commands.iter().all(|(argv, roles)| {
                    // Repository snapshots do not prove the effective default
                    // global Git excludes or ignores above the repository root.
                    // Nor can they prove the output of an external preprocessor.
                    !roles.follows_links
                        && roles.no_ignore_global
                        && roles.no_ignore_parent
                        && codex_shell_command::is_safe_command::is_known_safe_direct_argv(argv)
                }),
        },
    )))
}

// This optional cache must not turn a cheap search into an unbounded filesystem scan.
const SEARCH_SNAPSHOT_MAX_ENTRIES: usize = 512;
const SEARCH_SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(10);
static SEARCH_SNAPSHOT_WORKERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(2)));

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

#[cfg(test)]
pub(crate) async fn observe_rg_search_scope_state(search: &mut RgSearchNarrowing) {
    // Direct identity tests isolate admission; production shares the bounded pool.
    observe_rg_search_scope_state_with(
        search,
        false,
        Arc::new(Semaphore::new(1)),
        capture_search_scope_state,
    )
    .await;
}

pub(crate) async fn observe_rg_search_scope_state_with_freshness(
    search: &mut RgSearchNarrowing,
    force_fresh: bool,
) {
    observe_rg_search_scope_state_with(
        search,
        force_fresh,
        Arc::clone(&SEARCH_SNAPSHOT_WORKERS),
        capture_search_scope_state,
    )
    .await;
}

async fn observe_rg_search_scope_state_with(
    search: &mut RgSearchNarrowing,
    force_fresh: bool,
    workers: Arc<Semaphore>,
    mut capture: impl FnMut(&[PathBuf], &mut SearchSnapshotBudget) -> Option<String> + Send + 'static,
) {
    search.scope_state_identity = None;
    if force_fresh || !search.can_record_miss {
        return;
    }
    // Cache preparation is optional: never queue behind a slow scan. Keep the
    // permit on the worker until it actually exits, including after a timeout.
    let Ok(permit) = workers.try_acquire_owned() else {
        return;
    };
    let state_paths = search.state_paths.clone();
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let mut budget = SearchSnapshotBudget {
        remaining: SEARCH_SNAPSHOT_MAX_ENTRIES,
        deadline: Instant::now() + SEARCH_SNAPSHOT_TIMEOUT,
        cancellation,
    };
    let observation = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        capture_stable_search_scope_state(&state_paths, &mut budget, &mut capture)
    });
    // Dropping the wait cannot interrupt a filesystem call already in progress.
    // The drop guard cancels subsequent work, and late results are discarded.
    search.scope_state_identity = tokio::time::timeout(SEARCH_SNAPSHOT_TIMEOUT, observation)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
}

fn capture_stable_search_scope_state(
    state_paths: &[PathBuf],
    budget: &mut SearchSnapshotBudget,
    capture: &mut impl FnMut(&[PathBuf], &mut SearchSnapshotBudget) -> Option<String>,
) -> Option<String> {
    let first = capture(state_paths, budget)?;
    // Each traversal gets its entry budget, but shares the deadline and cancellation.
    budget.remaining = SEARCH_SNAPSHOT_MAX_ENTRIES;
    let second = capture(state_paths, budget)?;
    (first == second).then_some(first)
}

fn search_state_paths(
    targets: &[PathBuf],
    explicit_input_files: &[PathBuf],
    repository_root: &Path,
) -> Vec<PathBuf> {
    let mut paths = targets.to_vec();
    paths.extend(explicit_input_files.iter().cloned());
    let git_entry = repository_root.join(".git");
    // A linked worktree's .git file names its private metadata directory;
    // info/exclude belongs to the shared directory named by commondir.
    let git_dir = if git_entry.is_file() {
        paths.push(git_entry.clone());
        std::fs::read_to_string(&git_entry)
            .ok()
            .and_then(|text| text.trim().strip_prefix("gitdir: ").map(str::to_owned))
            .map(|path| repository_root.join(path))
            .unwrap_or_else(|| git_entry.clone())
    } else {
        git_entry
    };
    let common_dir_file = git_dir.join("commondir");
    paths.push(common_dir_file.clone());
    let common_dir = std::fs::read_to_string(common_dir_file)
        .map(|path| git_dir.join(path.trim()))
        .unwrap_or(git_dir);
    paths.push(common_dir.join("info").join("exclude"));
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
    // A target that does not exist yet falls back to lexical normalization, so
    // canonicalization must not add a `\\?\` verbatim prefix the fallback lacks.
    // Breadth compares these paths against the repository root, and the two
    // spellings never share a prefix: a not-yet-created path inside the
    // repository would otherwise be misread as a repository-wide search.
    dunce::canonicalize(path).unwrap_or_else(|_| {
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

pub(crate) fn rg_search_path_indices(argv: &[String]) -> Option<Vec<usize>> {
    let roles = RgArgumentRoles::parse(argv);
    roles.searches.then_some(roles.path_indices)
}

/// Bounded advisory candidates only; no search results are inferred from this
/// directory walk. The caller excludes Git-ignored candidates separately.
pub(crate) fn skipped_hidden_rg_directories(script: &str, cwd: &Path) -> Vec<PathBuf> {
    let Some(argv) = codex_shell_command::validation::standalone_argv(script) else { return Vec::new() };
    if !argv.first().is_some_and(|program| is_rg_program(program)) { return Vec::new(); }
    let roles = RgArgumentRoles::parse(&argv);
    if !roles.searches || roles.hidden { return Vec::new(); }
    let roots = if roles.path_indices.is_empty() { vec![cwd.to_path_buf()] } else {
        roles.path_indices.iter().map(|index| cwd.join(&argv[*index])).collect()
    };
    let mut found = std::collections::BTreeSet::new();
    let mut inspected = 0;
    for root in roots.into_iter().filter(|root| root.is_dir()) {
        let mut walk = walkdir::WalkDir::new(&root).follow_links(false).sort_by_file_name().into_iter();
        while let Some(entry) = walk.next() {
            inspected += 1;
            if inspected > 2048 || found.len() >= 16 { return found.into_iter().collect(); }
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 || !entry.file_type().is_dir() { continue; }
            if entry.file_name().to_string_lossy().starts_with('.') {
                walk.skip_current_dir();
                if entry.file_name() != ".git" { found.insert(entry.into_path()); }
            }
        }
    }
    found.into_iter().collect()
}

const MAX_MISSING_PATH_NOTES: usize = 3;

const MAX_PATH_SUGGESTIONS: usize = 3;


const MAX_SUGGESTION_DEPTH: usize = 3;
const MAX_SUGGESTION_ENTRIES: usize = 4_000;
const MAX_SUGGESTION_CLIMBS: usize = 2;

/// `rg` reports each search path it cannot open and still searches the rest.
/// A guessed path, such as a file this repository renamed, otherwise costs
/// another search, so name the closest existing paths. Advisory only: the
/// command and its results are unchanged.
pub(crate) fn missing_rg_path_advisory(output: &str, cwd: &Path) -> Option<String> {
    missing_rg_path_advisory_with_cancellation(output, cwd, CancellationToken::new())
}

pub(crate) fn missing_rg_path_advisory_with_cancellation(
    output: &str, cwd: &Path, cancellation: CancellationToken,
) -> Option<String> {
    let mut budget = suggestion_budget(cancellation);
    static MISSING_PATH: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"(?m)^rg: (.+?): [^\r\n]*\(os error [23]\)\r?$")
            .expect("valid rg missing-path regex")
    });
    let mut reported = Vec::<&str>::new();
    for capture in MISSING_PATH.captures_iter(output) {
        if budget.check().is_err() { break; }
        let path = capture.get(1).map_or("", |path| path.as_str()).trim();
        if !path.is_empty() && !reported.contains(&path) {
            reported.push(path);
        }
    }
    let notes = reported
        .into_iter()
        .filter_map(|reported| {
            let missing = cwd.join(reported);
            // A script can search from another directory; never contradict it.
            if missing.exists() {
                return None;
            }
            let suggestions = nearest_existing_paths_with_budget(&missing, cwd, &mut budget);
            (!suggestions.is_empty()).then(|| {
                let suggestions = suggestions
                    .iter()
                    .map(|path| format!("`{}`", display_suggestion(path, cwd)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("`{reported}` -> {suggestions}")
            })
        })
        .take(MAX_MISSING_PATH_NOTES)
        .collect::<Vec<_>>();
    (!notes.is_empty()).then(|| {
        format!(
            "Hint: `rg` could not open these search paths; possible existing paths (not verified replacements) are {}. The other paths still ran, and a path error is not evidence of no matches.",
            notes.join("; ")
        )
    })
}

/// Search below the deepest existing ancestor, climbing at most two levels and
/// never above the working directory for a path requested inside it.
pub(super) fn nearest_existing_paths(missing: &Path, cwd: &Path) -> Vec<PathBuf> {
    nearest_existing_paths_with_budget(missing, cwd, &mut suggestion_budget(CancellationToken::new()))
}

fn suggestion_budget(cancellation: CancellationToken) -> SearchSnapshotBudget {
    SearchSnapshotBudget {
        remaining: MAX_SUGGESTION_ENTRIES,
        deadline: Instant::now() + Duration::from_millis(25),
        cancellation,
    }
}

fn nearest_existing_paths_with_budget(
    missing: &Path, cwd: &Path, budget: &mut SearchSnapshotBudget,
) -> Vec<PathBuf> {
    let Some(name) = missing
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Vec::new();
    };
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&name)
        .to_string();
    let mut base = missing.parent();
    while let Some(directory) = base.filter(|directory| !directory.is_dir()) {
        if budget.check().is_err() { return Vec::new(); }
        base = directory.parent();
    }
    let within_cwd = missing.starts_with(cwd);
    let mut climbs = 0;
    while let Some(directory) = base {
        if budget.check().is_err() { break; }
        let mut candidates = suggestion_candidates(directory, &name, &stem, budget);
        if !candidates.is_empty() {
            candidates.sort();
            return candidates
                .into_iter()
                .take(MAX_PATH_SUGGESTIONS)
                .map(|(_, _, _, path)| path)
                .collect();
        }
        if climbs == MAX_SUGGESTION_CLIMBS || (within_cwd && directory == cwd) {
            break;
        }
        climbs += 1;
        base = directory.parent();
    }
    Vec::new()
}

/// Breadth-first so a bounded walk covers the nearest levels first; ranked by
/// match strength, then depth, then name length.
fn suggestion_candidates(
    base: &Path,
    name: &str,
    stem: &str,
    budget: &mut SearchSnapshotBudget,
) -> Vec<(std::cmp::Reverse<u8>, usize, usize, PathBuf)> {
    let mut candidates = Vec::new();
    let mut pending = std::collections::VecDeque::from([(base.to_path_buf(), 1)]);
    while let Some((directory, depth)) = pending.pop_front() {
        if budget.check().is_err() { break; }
        let Ok(mut entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        // Charge before advancing the iterator. Sorting an unbounded read_dir
        // first defeats the advisory budget on very large directories.
        loop {
            if budget.check().is_err() {
                return candidates;
            }
            let Some(entry) = entries.next() else { break; };
            let Ok(entry) = entry else { continue; };
            let Some(entry_name) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if let Some(score) = suggestion_score(&entry_name, is_dir, name, stem) {
                candidates.push((
                    std::cmp::Reverse(score),
                    depth,
                    entry_name.len(),
                    entry.path(),
                ));
            }
            if is_dir
                && depth < MAX_SUGGESTION_DEPTH
                && !matches!(entry_name.as_str(), ".git" | "target" | "node_modules")
            {
                pending.push_back((entry.path(), depth + 1));
            }
        }
    }
    candidates
}

/// Exact names first, then the same stem (`code_mode.rs` -> `code_mode/`),
/// then a stem that extends or shortens the request (`spec` -> `spec_plan`).
fn suggestion_score(candidate: &str, is_dir: bool, name: &str, stem: &str) -> Option<u8> {
    if candidate == name {
        return Some(3);
    }
    let candidate_stem = if is_dir {
        candidate
    } else {
        Path::new(candidate)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(candidate)
    };
    if candidate_stem == stem {
        return Some(2);
    }
    let extends = |longer: &str, shorter: &str| {
        shorter.len() >= 3
            && longer
                .strip_prefix(shorter)
                .is_some_and(|rest| rest.starts_with(['_', '-', '.']))
    };
    (extends(candidate_stem, stem) || extends(stem, candidate_stem)).then_some(1)
}

fn display_suggestion(path: &Path, cwd: &Path) -> String {
    let mut display = path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.to_string_lossy().replace('\\', "/"),
    );
    if path.is_dir() {
        display.push('/');
    }
    display
}

struct RgArgumentRoles<'a> {
    hidden: bool,
    unrestricted: usize,
    path_indices: Vec<usize>,
    input_files: Vec<&'a str>,
    searches: bool,
    follows_links: bool,
    no_ignore_global: bool,
    no_ignore_parent: bool,
    files_mode: bool,
    explicit_pattern: bool,
}

impl<'a> RgArgumentRoles<'a> {
    fn option(&mut self, flag: &str, value: Option<&'a str>) {
        match flag {
            "--hidden" => self.hidden = true,
            "--no-hidden" => self.hidden = false,
            "-u" | "--unrestricted" => {
                self.unrestricted += 1;
                self.no_ignore_global = true;
                self.no_ignore_parent = true;
                if self.unrestricted >= 2 { self.hidden = true; }
            }
            "--no-ignore" => {
                self.no_ignore_global = true;
                self.no_ignore_parent = true;
            }
            "--ignore" => {
                self.no_ignore_global = false;
                self.no_ignore_parent = false;
            }
            "--no-ignore-global" => self.no_ignore_global = true,
            "--ignore-global" => self.no_ignore_global = false,
            "--no-ignore-parent" => self.no_ignore_parent = true,
            "--ignore-parent" => self.no_ignore_parent = false,
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
            hidden: false,
            unrestricted: 0,
            path_indices: Vec::new(),
            input_files: Vec::new(),
            searches: true,
            follows_links: false,
            no_ignore_global: false,
            no_ignore_parent: false,
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

    #[test]
    fn hidden_rg_advisory_respects_operands_and_hidden_flags() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/.config")).unwrap();
        std::fs::create_dir_all(root.path().join("src/.git")).unwrap();
        std::fs::create_dir_all(root.path().join("elsewhere/.secret")).unwrap();
        assert_eq!(skipped_hidden_rg_directories("rg needle src --glob '*.toml'", root.path()),
            vec![root.path().join("src/.config")]);
        for command in ["rg --hidden needle src", "rg -uu needle src", "rg -uuu needle src",
            "rg --help", "echo rg needle src", "rg needle src; echo done"] {
            assert!(skipped_hidden_rg_directories(command, root.path()).is_empty(), "{command}");
        }
    }

    #[test]
    fn linked_worktree_snapshot_tracks_shared_exclude_and_gitdir_redirects() {
        let fixture = tempfile::tempdir().unwrap();
        let main_git = fixture.path().join("main/.git");
        let worktree_git = main_git.join("worktrees/linked");
        let linked = fixture.path().join("linked");
        std::fs::create_dir_all(main_git.join("info")).unwrap();
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::create_dir_all(linked.join("src")).unwrap();
        std::fs::write(linked.join("src/file.txt"), "needle").unwrap();
        std::fs::write(
            linked.join(".git"),
            "gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
        let exclude = main_git.join("info/exclude");
        std::fs::write(&exclude, "file.txt\n").unwrap();
        let snapshot = || {
            let search = classify_rg_search_narrowing(
                &["rg".into(), "needle".into(), "src".into()],
                None,
                &linked,
                &linked,
            )
            .unwrap()
            .unwrap();
            capture_search_scope_state(
                &search.state_paths,
                &mut SearchSnapshotBudget {
                    remaining: SEARCH_SNAPSHOT_MAX_ENTRIES,
                    deadline: Instant::now() + Duration::from_secs(10),
                    cancellation: CancellationToken::new(),
                },
            )
            .expect("small linked worktree has complete scope evidence")
        };
        let before = snapshot();
        std::fs::write(&exclude, "another-file.txt\n").unwrap();
        assert_ne!(
            before,
            snapshot(),
            "the shared exclude changes search results"
        );
        let before_redirect = snapshot();
        std::fs::write(worktree_git.join("commondir"), "../../alternate\n").unwrap();
        assert_ne!(
            before_redirect,
            snapshot(),
            "metadata redirects are dependencies too"
        );
    }

    #[tokio::test]
    async fn explicitly_fresh_search_skips_scope_capture() {
        let command = ["rg", "--no-ignore-global", "--no-ignore-parent", "needle", "src"]
            .map(str::to_string).to_vec();
        let mut search = classify_rg_search_narrowing(
            &command,
            None,
            Path::new("workspace"),
            Path::new("workspace"),
        )
        .unwrap()
        .unwrap();
        assert!(search.can_record_miss, "freshness must be the only capture bypass");
        search.scope_state_identity = Some("old evidence".to_string());
        let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_captures = Arc::clone(&captures);
        observe_rg_search_scope_state_with(
            &mut search,
            true,
            Arc::new(Semaphore::new(1)),
            move |_, _| {
                captures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some("unexpected scan".to_string())
            },
        )
        .await;
        assert_eq!(search.scope_state_identity, None);
        assert_eq!(
            observed_captures.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(search.breadth, RgSearchBreadth::Narrow);
    }

    #[tokio::test]
    async fn timed_out_worker_keeps_admission_until_it_exits() {
        let command = ["rg", "--no-ignore-global", "--no-ignore-parent", "needle", "src"].map(str::to_string).to_vec();
        let mut search = classify_rg_search_narrowing(
            &command,
            None,
            Path::new("workspace"),
            Path::new("workspace"),
        )
        .unwrap()
        .unwrap();
        let mut next_search = search.clone();
        let workers = Arc::new(Semaphore::new(1));
        let scan_workers = Arc::clone(&workers);
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let observation = tokio::spawn(async move {
            observe_rg_search_scope_state_with(
                &mut search,
                false,
                scan_workers,
                move |_, budget| {
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                        blocked.recv().unwrap();
                    }
                    budget.check().ok()?;
                    Some("late evidence".to_string())
                },
            )
            .await;
            search
        });
        ready.await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), observation).await;
        let available_while_blocked = workers.available_permits();
        let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_captures = Arc::clone(&captures);
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            observe_rg_search_scope_state_with(
                &mut next_search,
                false,
                Arc::clone(&workers),
                move |_, _| {
                    captures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some("unexpected scan".to_string())
                },
            ),
        )
        .await;
        // Release blocked I/O before any assertions, even if admission regresses.
        let _ = release.send(());
        assert_eq!(result.unwrap().unwrap().scope_state_identity, None);
        assert_eq!(available_while_blocked, 0);
        second.expect("saturated admission must skip optional work without waiting");
        assert_eq!(
            observed_captures.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(next_search.scope_state_identity, None);
        let permit = tokio::time::timeout(Duration::from_secs(1), workers.acquire())
            .await
            .expect("worker exit must release admission")
            .unwrap();
        drop(permit);
        assert_eq!(workers.available_permits(), 1);
    }

    #[test]
    fn stable_scope_can_use_the_full_entry_budget_in_both_captures() {
        let mut captures = 0;
        let identity = capture_stable_search_scope_state(
            &[],
            &mut SearchSnapshotBudget {
                remaining: SEARCH_SNAPSHOT_MAX_ENTRIES,
                deadline: Instant::now() + Duration::from_secs(10),
                cancellation: CancellationToken::new(),
            },
            &mut |_, budget| {
                for _ in 0..SEARCH_SNAPSHOT_MAX_ENTRIES {
                    budget.check().ok()?;
                }
                captures += 1;
                Some("stable scope".to_string())
            },
        );

        assert_eq!(captures, 2);
        assert_eq!(identity.as_deref(), Some("stable scope"));
    }

    #[test]
    fn changed_scope_is_not_reusable_between_captures() {
        let mut captures = 0;
        let identity = capture_stable_search_scope_state(
            &[],
            &mut SearchSnapshotBudget {
                remaining: SEARCH_SNAPSHOT_MAX_ENTRIES,
                deadline: Instant::now() + Duration::from_secs(10),
                cancellation: CancellationToken::new(),
            },
            &mut |_, budget| {
                budget.check().ok()?;
                captures += 1;
                Some(format!("scope revision {captures}"))
            },
        );

        assert_eq!(captures, 2);
        assert_eq!(identity, None);
    }

    #[tokio::test]
    async fn slow_scope_observation_releases_the_request_and_discards_late_evidence() {
        let command = ["rg", "--no-ignore-global", "--no-ignore-parent", "needle", "src"].map(str::to_string).to_vec();
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
            observe_rg_search_scope_state_with(
                &mut search,
                false,
                Arc::new(Semaphore::new(1)),
                move |_, budget| {
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                        blocked.recv().unwrap();
                        let _ = finished
                            .take()
                            .unwrap()
                            .send(budget.cancellation.is_cancelled());
                    }
                    Some("late evidence".to_string())
                },
            )
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

#[cfg(test)]
mod missing_path_tests {
    use super::*;

    #[test]
    fn suggestion_enumeration_stops_at_budget_and_cancellation() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..100 {
            std::fs::write(root.path().join(format!("candidate_{index}.rs")), "").unwrap();
        }
        let mut budget = suggestion_budget(CancellationToken::new());
        budget.remaining = 3;
        budget.deadline = Instant::now() + Duration::from_secs(10);
        let candidates = suggestion_candidates(root.path(), "candidate.rs", "candidate", &mut budget);
        assert_eq!(candidates.len(), 2);
        assert_eq!(budget.remaining, 0);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(suggestion_candidates(root.path(), "candidate.rs", "candidate",
            &mut suggestion_budget(cancellation)).is_empty());
    }

    #[test]
    fn missing_rg_paths_name_the_nearest_existing_paths() {
        let root = tempfile::tempdir().unwrap();
        for file in [
            "codex-rs/core/src/tools/spec_plan.rs",
            "codex-rs/core/src/tools/code_mode/mod.rs",
            "codex-rs/core/src/tools/handlers/shell.rs",
            "justfile",
        ] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        // Misses recorded in real sessions: a renamed file, a module that moved
        // directories, a file one level up, and a path that exists nowhere.
        let output = concat!(
            "codex-rs/core/src/tools/spec_plan.rs:1:needle\n",
            "rg: codex-rs/core/src/tools/spec.rs: The system cannot find the file specified. (os error 2)\r\n",
            "rg: codex-rs/core/src/tools/handlers/code_mode.rs: IO error for operation on codex-rs/core/src/tools/handlers/code_mode.rs: No such file or directory (os error 2)\n",
            "rg: codex-rs/justfile: No such file or directory (os error 2)\n",
            "rg: .github: The system cannot find the path specified. (os error 3)\n",
        );
        let advisory = missing_rg_path_advisory(output, root.path()).expect("advisory");
        for expected in [
            "`codex-rs/core/src/tools/spec.rs` -> `codex-rs/core/src/tools/spec_plan.rs`",
            "`codex-rs/core/src/tools/handlers/code_mode.rs` -> `codex-rs/core/src/tools/code_mode/`",
            "`codex-rs/justfile` -> `justfile`",
        ] {
            assert!(advisory.contains(expected), "{advisory}");
        }
        assert!(!advisory.contains(".github"), "{advisory}");
        // No hint without an rg path error, or for a path that exists here.
        assert_eq!(missing_rg_path_advisory("src/a.rs:1:needle\n", root.path()), None);
        assert_eq!(
            missing_rg_path_advisory(
                "rg: justfile: No such file or directory (os error 2)\n",
                root.path()
            ),
            None
        );
    }
}
