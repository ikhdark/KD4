//! Unmerged index entries are authoritative; worktree markers are only hints.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;

use crate::historical::ReadCancellation;
use crate::historical::read_git_command;
use crate::historical::read_git_command_with_input;
use crate::historical::relative_git_path;
use crate::historical::valid_oid;
use crate::inspection::MAX_INSPECTION_ENTRIES;
use crate::inspection::bounded_warnings;
use crate::inspection::repository_root;
use crate::operations::GitCommandControl;

const CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
const MARKER_BYTE_BUDGET: usize = 32 * 1024 * 1024;

#[derive(Serialize)]
pub struct ConflictStage {
    /// 1 = base, 2 = ours, 3 = theirs. Missing stages remain absent.
    pub stage: u8,
    pub mode: String,
    pub oid: String,
}

#[derive(Serialize)]
pub struct ConflictMarker {
    pub line: usize,
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct ConflictMarkerScan {
    pub status: &'static str,
    pub marker_size: usize,
    pub bytes_scanned: usize,
    pub markers: Vec<ConflictMarker>,
    pub markers_truncated: bool,
    pub error: Option<String>,
}

impl ConflictMarkerScan {
    fn new(status: &'static str, marker_size: usize) -> Self {
        Self {
            status,
            marker_size,
            bytes_scanned: 0,
            markers: Vec::new(),
            markers_truncated: false,
            error: None,
        }
    }
}

#[derive(Serialize)]
pub struct GitConflict {
    pub path: String,
    pub stages: Vec<ConflictStage>,
    pub marker_scan: ConflictMarkerScan,
}

#[derive(Serialize)]
pub struct GitConflicts {
    pub repository_root: PathBuf,
    pub paths: Vec<String>,
    pub total_files: usize,
    pub files: Vec<GitConflict>,
    pub truncated: bool,
    pub coherent_snapshot_verified: bool,
    pub warnings: String,
}

/// Only read commands are issued; ls-files never refreshes or stages the index.
/// Dropping the future cancels the existing bounded Git process owner and scan.
pub async fn inspect_conflicts(
    cwd: PathBuf,
    paths: Vec<String>,
    max_files: usize,
    max_markers: usize,
) -> Result<GitConflicts, String> {
    if !(1..=MAX_INSPECTION_ENTRIES).contains(&max_files)
        || !(1..=MAX_INSPECTION_ENTRIES).contains(&max_markers)
        || paths.len() > 64
    {
        return Err(
            "Git conflicts requires max_files/max_markers 1..1000 and at most 64 paths".into(),
        );
    }
    let paths = paths
        .iter()
        .map(|path| relative_git_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || {
        let root = repository_root(&cwd, &control)?;
        let mut args = vec!["--literal-pathspecs", "ls-files", "--unmerged", "-z", "--"];
        args.extend(paths.iter().map(String::as_str));
        let output = read_git_command(&root, &args, CAPTURE_LIMIT, &control)?;
        if !output.status.success() {
            return Err(format!(
                "Git unmerged index read failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let mut entries = parse_unmerged(&output.stdout)?;
        let total_files = entries.len();
        entries.truncate(max_files);
        let mut warnings = bounded_warnings(&output.stderr);
        let sizes = marker_sizes(&root, &entries, &control);
        if let Ok((_, stderr)) = &sizes {
            warnings.push_str(stderr);
            warnings = bounded_warnings(warnings.as_bytes());
        }
        let mut budget = MARKER_BYTE_BUDGET;
        for entry in &mut entries {
            control.check().map_err(|error| error.to_string())?;
            entry.marker_scan = match &sizes {
                Ok((sizes, _)) => scan_markers(
                    &root,
                    &entry.path,
                    sizes.get(&entry.path).copied().unwrap_or(7),
                    max_markers,
                    &mut budget,
                    &control,
                ),
                Err(error) => {
                    let mut scan = ConflictMarkerScan::new("unreadable", 7);
                    scan.error = Some(format!(
                        "Cannot read conflict-marker-size attributes: {error}"
                    ));
                    scan
                }
            };
        }
        control.check().map_err(|error| error.to_string())?;
        Ok(GitConflicts {
            repository_root: root,
            paths,
            total_files,
            files: entries,
            truncated: total_files > max_files,
            coherent_snapshot_verified: false,
            warnings,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

fn parse_unmerged(bytes: &[u8]) -> Result<Vec<GitConflict>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "Git conflicts requires UTF-8 paths")?;
    let text = text
        .strip_suffix('\0')
        .ok_or("Unterminated unmerged index record")?;
    let mut files: BTreeMap<String, Vec<ConflictStage>> = BTreeMap::new();
    for record in text.split('\0') {
        let (header, path) = record
            .split_once('\t')
            .ok_or("Invalid unmerged index record")?;
        let fields = header.split(' ').collect::<Vec<_>>();
        if fields.len() != 3
            || fields[0].len() != 6
            || !fields[0].bytes().all(|byte| (b'0'..=b'7').contains(&byte))
            || !valid_oid(fields[1])
            || !matches!(fields[2], "1" | "2" | "3")
            || path.is_empty()
        {
            return Err("Invalid unmerged index fields".into());
        }
        let stage = fields[2].as_bytes()[0] - b'0';
        let stages = files.entry(path.to_string()).or_default();
        if stages.iter().any(|entry| entry.stage == stage) {
            return Err("Duplicate unmerged index stage".into());
        }
        stages.push(ConflictStage {
            stage,
            mode: fields[0].into(),
            oid: fields[1].into(),
        });
    }
    Ok(files
        .into_iter()
        .map(|(path, mut stages)| {
            stages.sort_by_key(|entry| entry.stage);
            GitConflict {
                path,
                stages,
                marker_scan: ConflictMarkerScan::new("unreadable", 7),
            }
        })
        .collect())
}

fn marker_sizes(
    root: &Path,
    entries: &[GitConflict],
    control: &GitCommandControl,
) -> Result<(BTreeMap<String, usize>, String), String> {
    let mut sizes = BTreeMap::new();
    if entries.is_empty() {
        return Ok((sizes, String::new()));
    }
    let input = entries
        .iter()
        .flat_map(|entry| entry.path.bytes().chain([0]))
        .collect::<Vec<_>>();
    let output = read_git_command_with_input(
        root,
        &["check-attr", "-z", "--stdin", "conflict-marker-size"],
        None,
        &[],
        CAPTURE_LIMIT,
        control,
        Some(&input),
    )?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "Non-UTF-8 Git attributes")?;
    let fields = text
        .strip_suffix('\0')
        .ok_or("Unterminated Git attributes")?
        .split('\0')
        .collect::<Vec<_>>();
    if fields.len() != entries.len() * 3 {
        return Err("Incomplete Git conflict-marker-size attributes".into());
    }
    for (fields, entry) in fields.chunks_exact(3).zip(entries) {
        if fields[0] != entry.path || fields[1] != "conflict-marker-size" {
            return Err("Mismatched Git conflict-marker-size attributes".into());
        }
        let size = fields[2]
            .parse::<usize>()
            .ok()
            .filter(|size| *size > 0)
            .unwrap_or(7);
        sizes.insert(entry.path.clone(), size);
    }
    Ok((sizes, bounded_warnings(&output.stderr)))
}

fn scan_markers(
    root: &Path,
    path: &str,
    marker_size: usize,
    max_markers: usize,
    budget: &mut usize,
    control: &GitCommandControl,
) -> ConflictMarkerScan {
    let mut scan = ConflictMarkerScan::new("complete", marker_size);
    // Do not intentionally follow symlinks/junctions or open special files.
    let mut absolute = root.to_path_buf();
    let components = Path::new(path).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        if !matches!(component, Component::Normal(_))
            || component.as_os_str().to_string_lossy().contains(':')
        {
            scan.status = "not_regular_file";
            return scan;
        }
        absolute.push(component);
        match std::fs::symlink_metadata(&absolute) {
            Ok(metadata)
                if !metadata.file_type().is_symlink()
                    && (if index + 1 == components.len() {
                        metadata.is_file()
                    } else {
                        metadata.is_dir()
                    }) => {}
            Ok(_) => {
                scan.status = "not_regular_file";
                return scan;
            }
            Err(error) => {
                scan.status = if error.kind() == std::io::ErrorKind::NotFound {
                    "missing"
                } else {
                    "unreadable"
                };
                scan.error = Some(error.to_string());
                return scan;
            }
        }
    }
    if *budget == 0 {
        scan.status = "budget_limit";
        return scan;
    }
    let limit = (CAPTURE_LIMIT + 1).min(*budget);
    let mut bytes = Vec::new();
    let read = (|| -> std::io::Result<bool> {
        let mut file = File::open(absolute)?;
        let mut buffer = [0; 16 * 1024];
        while bytes.len() < limit {
            control.check()?;
            let count = file.read(&mut buffer[..(limit - bytes.len()).min(16 * 1024)])?;
            if count == 0 {
                return Ok(true);
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        Ok(false)
    })();
    *budget -= bytes.len();
    scan.bytes_scanned = bytes.len().min(CAPTURE_LIMIT);
    match read {
        Err(error) => {
            scan.status = "unreadable";
            scan.error = Some(error.to_string());
            return scan;
        }
        Ok(false) => {
            scan.status = if limit > CAPTURE_LIMIT {
                "byte_limit"
            } else {
                "budget_limit"
            }
        }
        Ok(true) => {}
    }
    bytes.truncate(CAPTURE_LIMIT);
    if bytes.contains(&0) {
        scan.status = "binary";
        return scan;
    }
    let mut offset = 0;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        // An incomplete last line cannot certify a complete marker delimiter.
        offset += line.len() + 1;
        if scan.status != "complete" && offset > bytes.len() {
            break;
        }
        let line = if index == 0 {
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        } else {
            line
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(first) = line.first() else {
            continue;
        };
        let kind = match first {
            b'<' => "start",
            b'|' => "base",
            b'=' => "separator",
            b'>' => "end",
            _ => continue,
        };
        let width = line.iter().take_while(|byte| *byte == first).count();
        if width != marker_size
            || !(line.len() == width || (*first != b'=' && line.get(width) == Some(&b' ')))
        {
            continue;
        }
        if scan.markers.len() < max_markers {
            scan.markers.push(ConflictMarker {
                line: index + 1,
                kind,
            });
        } else {
            scan.markers_truncated = true;
        }
    }
    scan
}
