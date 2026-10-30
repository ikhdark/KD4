use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;

use crate::historical::ReadCancellation;
use crate::historical::disabled_filter_config;
use crate::historical::read_git;
use crate::historical::read_git_command_with;
use crate::historical::read_git_lossy;
use crate::historical::relative_git_path;
use crate::historical::resolve_read_revision;
use crate::historical::valid_oid;
use crate::operations::GitCommandControl;

pub const MAX_BLAME_LINES: usize = 200;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BLAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlameSource {
    Revision,
    WorkingTree,
}

#[derive(Serialize)]
pub struct BlameRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlameAttribution {
    Committed,
    WorkingTree,
    UnavailableHistory,
}

#[derive(Serialize)]
pub struct BlameGroup {
    pub range: BlameRange,
    pub attribution: BlameAttribution,
    pub commit_oid: Option<String>,
    pub original_start: Option<usize>,
    pub original_path: Option<String>,
    pub boundary: bool,
}

#[derive(Serialize)]
pub struct BlameResult {
    pub repository_root: Option<PathBuf>,
    pub path: String,
    pub revision: String,
    pub source: BlameSource,
    pub commit_oid: Option<String>,
    /// Identity of the blamed content (working-tree bytes after Git's built-in
    /// EOL normalization); working-tree objects are not written.
    pub blob_oid: Option<String>,
    pub range: BlameRange,
    pub range_complete: bool,
    pub history_complete: bool,
    pub groups: Vec<BlameGroup>,
    pub unavailable_reason: Option<String>,
}

/// Attribution only: not a source-read receipt or proof of semantic authorship.
/// Shares the bounded process/Job owner and cancellation with historical reads.
pub async fn blame_range(
    cwd: PathBuf,
    path: String,
    revision: String,
    source: BlameSource,
    start: usize,
    end: usize,
) -> Result<BlameResult, String> {
    let path = relative_git_path(&path)?;
    if start == 0 || end < start || end - start >= MAX_BLAME_LINES {
        return Err(format!(
            "blame requires a one-based inclusive range of 1-{MAX_BLAME_LINES} lines"
        ));
    }
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || {
        let mut result = BlameResult {
            repository_root: None,
            path,
            revision,
            source,
            commit_oid: None,
            blob_oid: None,
            range: BlameRange { start, end },
            range_complete: false,
            history_complete: false,
            groups: Vec::new(),
            unavailable_reason: None,
        };
        if let Err(error) = capture_blame(&cwd, &mut result, &control) {
            result.groups.clear();
            result.unavailable_reason = Some(error);
        }
        result
    })
    .await
    .map_err(|error| error.to_string())
}

fn capture_blame(
    cwd: &Path,
    result: &mut BlameResult,
    control: &GitCommandControl,
) -> Result<(), String> {
    let root = read_git(cwd, &["rev-parse", "--show-toplevel"], 32 * 1024, control)?;
    let root = PathBuf::from(root.trim_end_matches(['\r', '\n']));
    result.repository_root = Some(root.clone());
    let commit = resolve_read_revision(&root, &result.revision, control)?;
    result.commit_oid = Some(commit.clone());
    let run = |args: &[&str], limit| read_git(&root, args, limit, control);
    let entry = run(
        &[
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            &commit,
            "--",
            &result.path,
        ],
        32 * 1024,
    )?;
    let blob = if entry.is_empty() {
        None
    } else {
        let (header, name) = entry
            .strip_suffix('\0')
            .and_then(|entry| entry.split_once('\t'))
            .ok_or("invalid Git tree entry")?;
        let fields = header.split(' ').collect::<Vec<_>>();
        if name != result.path
            || fields.len() != 3
            || !matches!(fields[0], "100644" | "100755")
            || fields[1] != "blob"
            || !valid_oid(fields[2])
        {
            return Err("blame requires a regular file, not a tree, symlink or submodule".into());
        }
        Some(fields[2].to_string())
    };
    let working = result.source == BlameSource::WorkingTree;
    // An uncommitted rename is absent from the baseline tree but tracked by the
    // index; Git follows it to its committed source like any tracked path.
    let mut indexed = false;
    if working && blob.is_none() {
        let entries = run(
            &[
                "--literal-pathspecs",
                "ls-files",
                "-z",
                "--stage",
                "--",
                &result.path,
            ],
            MAX_BLAME_BYTES,
        )?;
        for entry in entries.split_terminator('\0') {
            let (header, name) = entry.split_once('\t').ok_or("invalid Git index entry")?;
            if name == result.path {
                if !header.starts_with("100644 ") && !header.starts_with("100755 ") {
                    return Err(
                        "blame requires a regular file, not a tree, symlink or submodule".into(),
                    );
                }
                indexed = true;
            }
        }
    }
    let tracked = blob.is_some() || indexed;
    if working && tracked {
        // Git adds MERGE_HEAD as another parent of the working-tree commit,
        // crediting commits outside the single resolved baseline.
        let merge_head = run(&["rev-parse", "--git-path", "MERGE_HEAD"], 32 * 1024)?;
        if root
            .join(merge_head.trim_end_matches(['\r', '\n']))
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            return Err("working-tree attribution is unavailable while a merge is in progress (MERGE_HEAD exists); use committed attribution".into());
        }
    }
    // --contents is fed a captured private file, never a live path or a filter.
    // Even working-tree attribution uses the single resolved commit as baseline.
    let mut snapshot = None;
    let contents = if working {
        // --no-textconv does not disable clean/encoding/ident conversion of
        // --contents. Refuse those attributes rather than execute a filter;
        // Git's built-in EOL normalization applies, as it does for git_diff.
        let attributes = run(
            &[
                "check-attr",
                "-z",
                "filter",
                "working-tree-encoding",
                "ident",
                "--",
                &result.path,
            ],
            32 * 1024,
        )?;
        let fields = attributes.split_terminator('\0').collect::<Vec<_>>();
        if fields.len() != 9
            || fields
                .as_chunks::<3>()
                .0
                .iter()
                .any(|&[_, _, value]| !matches!(value, "unspecified" | "unset"))
        {
            return Err("working-tree attribution is unavailable for files with filter, working-tree-encoding or ident attributes; use committed attribution".into());
        }
        // check-attr and blame read the index copy of a .gitattributes that
        // Git cannot read from the working tree; hash-object --path never
        // loads the index. Refuse when those attribute views can differ.
        let attribute_files = std::iter::once(".gitattributes".to_string())
            .chain(
                result
                    .path
                    .match_indices('/')
                    .map(|(end, _)| format!("{}/.gitattributes", &result.path[..end])),
            )
            .collect::<Vec<_>>();
        let mut ls_files = vec!["--literal-pathspecs", "ls-files", "-z", "--cached", "--"];
        ls_files.extend(attribute_files.iter().map(String::as_str));
        let indexed_attributes = read_git_lossy(&root, &ls_files, MAX_BLAME_BYTES, control)?;
        for name in indexed_attributes
            .split_terminator('\0')
            .filter(|name| attribute_files.iter().any(|file| file == name))
        {
            // Git reads the working-tree copy only as a regular file below 100 MiB.
            let file = root.join(name);
            let readable = std::fs::symlink_metadata(&file)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() < 100 * 1024 * 1024)
                && std::fs::File::open(file).is_ok();
            if !readable {
                return Err(format!("working-tree attribution is unavailable while {name} is in the index but missing or unreadable in the working tree; use committed attribution"));
            }
        }
        let path = root.join(&result.path);
        let canonical_root = root.canonicalize().map_err(|error| error.to_string())?;
        let canonical_path = path.canonicalize().map_err(|error| error.to_string())?;
        let Ok(inside) = canonical_path.strip_prefix(&canonical_root) else {
            return Err("working-tree blame requires a regular file inside the repository".into());
        };
        if !std::fs::symlink_metadata(&path)
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("working-tree blame requires a regular file inside the repository".into());
        }
        if !tracked {
            // Untracked bytes have no history to follow. Require the exact
            // on-disk name and this repository as owner, so case aliases,
            // symlinked directories, submodules and nested repositories never
            // pose as new uncommitted files.
            let exact = inside
                .iter()
                .eq(result.path.split('/').map(std::ffi::OsStr::new));
            let parent = path.parent().ok_or("invalid working-tree path")?;
            let owner = read_git(parent, &["rev-parse", "--show-toplevel"], 32 * 1024, control)?;
            let owner = PathBuf::from(owner.trim_end_matches(['\r', '\n']))
                .canonicalize()
                .map_err(|error| error.to_string())?;
            if !exact || owner != canonical_root {
                return Err("path is neither tracked at the baseline commit or in the index nor exactly an untracked file of this repository (check case, symlinked directories and submodules)".into());
            }
        }
        let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err("working-tree blame exceeds the 8 MiB file limit".into());
        }
        let mut temp = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
        temp.write_all(&bytes).map_err(|error| error.to_string())?;
        let name = temp.path().to_str().ok_or("snapshot path is not UTF-8")?;
        let has_cr = bytes.contains(&b'\r');
        result.blob_oid = Some(working_blob_oid(&root, &result.path, name, has_cr, control)?);
        snapshot = Some(temp);
        String::from_utf8(bytes).map_err(|_| "working-tree blame requires UTF-8 text")?
    } else {
        let blob = blob
            .as_deref()
            .ok_or("path is missing at the resolved commit")?;
        let size = run(&["cat-file", "-s", blob], 1024)?
            .trim()
            .parse::<usize>()
            .map_err(|_| "invalid blob size")?;
        if size > MAX_FILE_BYTES {
            return Err("blame exceeds the 8 MiB file limit".into());
        }
        result.blob_oid = Some(blob.to_string());
        run(&["cat-file", "blob", blob], MAX_FILE_BYTES)?
    };
    if contents.contains('\0') {
        return Err("blame requires text without NUL bytes".into());
    }
    let lines = contents.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!contents.is_empty() && !contents.ends_with('\n'));
    if result.range.end > lines {
        return Err(format!("requested range exceeds the {lines}-line source"));
    }
    if !tracked {
        result.groups.push(BlameGroup {
            range: BlameRange {
                start: result.range.start,
                end: result.range.end,
            },
            attribution: BlameAttribution::WorkingTree,
            commit_oid: None,
            original_start: None,
            original_path: None,
            boundary: false,
        });
    } else {
        let range = format!("{},{}", result.range.start, result.range.end);
        // Neutralize per-user settings that change attribution: ignore-revs
        // files credit other commits; blame.showRoot hides shallow boundaries.
        // Quoted paths keep filename records ASCII while other metadata (author,
        // summary) may be in a legacy encoding.
        let mut args = vec![
            "-c",
            "core.quotePath=true",
            "blame",
            "--incremental",
            "--no-textconv",
            "--no-progress",
            "--no-ignore-revs-file",
            "--no-root",
            "-L",
            &range,
        ];
        if let Some(snapshot) = &snapshot {
            args.extend([
                "--contents",
                snapshot
                    .path()
                    .to_str()
                    .ok_or("snapshot path is not UTF-8")?,
            ]);
        }
        args.extend([&commit, "--", &result.path]);
        let output = read_git_lossy(&root, &args, MAX_BLAME_BYTES, control)?;
        result.groups = parse_blame(&output)?;
        result.groups.sort_by_key(|group| group.range.start);
        let mut next = result.range.start;
        let mut boundaries = HashMap::new();
        for group in &mut result.groups {
            if group.range.start != next || group.range.end > result.range.end {
                return Err("Git blame returned incomplete or overlapping ranges".into());
            }
            next = group.range.end + 1;
            if group.boundary
                && let Some(oid) = &group.commit_oid
            {
                let missing = if let Some(missing) = boundaries.get(oid) {
                    *missing
                } else {
                    // Raw commit parents distinguish a true root from shallow/grafted history.
                    let object = read_git_lossy(
                        &root,
                        &["cat-file", "commit", oid],
                        MAX_BLAME_BYTES,
                        control,
                    )?;
                    let missing = object
                        .split("\n\n")
                        .next()
                        .unwrap_or("")
                        .lines()
                        .any(|line| line.starts_with("parent "));
                    boundaries.insert(oid.clone(), missing);
                    missing
                };
                if missing {
                    group.attribution = BlameAttribution::UnavailableHistory;
                }
            }
        }
        if next != result.range.end + 1 {
            return Err("Git blame did not cover the requested range".into());
        }
    }
    result.range_complete = true;
    result.history_complete = !result
        .groups
        .iter()
        .any(|group| matches!(group.attribution, BlameAttribution::UnavailableHistory));
    if !result.history_complete {
        result.unavailable_reason = Some("history stops at a boundary with unavailable parents; boundary commit IDs are observations, not definitive origins".into());
    }
    Ok(())
}

/// Identity of the content Git blames for captured working-tree bytes: its
/// built-in EOL normalization, except that auto conversion (text=auto or
/// core.autocrlf) keeps bytes whose index copy already has CRLF ("safer
/// autocrlf"), which hash-object alone does not consult.
fn working_blob_oid(
    root: &Path,
    path: &str,
    snapshot: &str,
    has_cr: bool,
    control: &GitCommandControl,
) -> Result<String, String> {
    let hash = |conversion: &str, config: &[(String, &'static str)]| -> Result<String, String> {
        let output = read_git_command_with(
            root,
            &["hash-object", conversion, "--", snapshot],
            None,
            config,
            1024,
            control,
        )?;
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() && valid_oid(&oid) {
            Ok(oid)
        } else {
            Err(format!(
                "invalid working-tree blob identity: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    };
    let raw = hash("--no-filters", &[])?;
    // Filter/encoding/ident conversions were refused; EOL conversion only
    // removes CRs.
    if !has_cr {
        return Ok(raw);
    }
    // Configured drivers stay disabled even if attributes changed after
    // check-attr: --path conversion never runs a filter.
    let normalized = hash(
        format!("--path={path}").as_str(),
        &disabled_filter_config(root, control)?,
    )?;
    if raw == normalized {
        return Ok(raw);
    }
    let eol = read_git_lossy(
        root,
        &["--literal-pathspecs", "ls-files", "-z", "--eol", "--", path],
        MAX_BLAME_BYTES,
        control,
    )?;
    let unconverted = eol.split_terminator('\0').any(|entry| {
        entry.split_once('\t').is_some_and(|(info, name)| {
            let attribute = info
                .split_once("attr/")
                .map_or("", |(_, attribute)| attribute.trim_end());
            name == path
                && (info.starts_with("i/crlf ") || info.starts_with("i/mixed "))
                && (attribute.is_empty() || attribute.starts_with("text=auto"))
        })
    });
    Ok(if unconverted { raw } else { normalized })
}

fn parse_blame(output: &str) -> Result<Vec<BlameGroup>, String> {
    let mut lines = output.lines();
    let mut groups = Vec::new();
    let mut boundaries = HashMap::new();
    while let Some(header) = lines.next() {
        let fields = header.split(' ').collect::<Vec<_>>();
        if fields.len() != 4 || !valid_oid(fields[0]) {
            return Err("invalid Git blame header".into());
        }
        let number = |value: &str| {
            value
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or("invalid Git blame line number")
        };
        let original = number(fields[1])?;
        let start = number(fields[2])?;
        let count = number(fields[3])?;
        let end = start
            .checked_add(count - 1)
            .ok_or("Git blame range overflow")?;
        let oid = fields[0];
        let working = oid.bytes().all(|byte| byte == b'0');
        let mut boundary = *boundaries.get(oid).unwrap_or(&false);
        let mut path = None;
        for line in lines.by_ref() {
            if line == "boundary" {
                boundary = true;
            }
            if let Some(name) = line.strip_prefix("filename ") {
                path = Some(
                    if let Some(quoted) = name
                        .strip_prefix('"')
                        .and_then(|name| name.strip_suffix('"'))
                    {
                        String::from_utf8(crate::unescape_c_bytes(quoted))
                            .map_err(|_| "Git blame path is not UTF-8")?
                    } else {
                        name.to_string()
                    },
                );
                break;
            }
        }
        let path = path.ok_or("Git blame omitted the original path")?;
        boundaries.insert(oid.to_string(), boundary);
        groups.push(BlameGroup {
            range: BlameRange { start, end },
            attribution: if working {
                BlameAttribution::WorkingTree
            } else {
                BlameAttribution::Committed
            },
            commit_oid: (!working).then(|| oid.to_string()),
            original_start: (!working).then_some(original),
            original_path: (!working).then_some(path),
            boundary,
        });
    }
    Ok(groups)
}
