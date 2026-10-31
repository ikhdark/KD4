use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;

use crate::historical::ReadCancellation;
use crate::historical::read_git;
use crate::historical::read_git_command_with_input;
use crate::historical::relative_git_path;
use crate::operations::GitCommandControl;

const CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
const HEADER_LIMIT: u64 = 8192;

#[derive(Debug, Serialize)]
pub struct ClassificationEvidence {
    pub source: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct PathLabel {
    pub value: Option<bool>,
    pub evidence: Vec<ClassificationEvidence>,
    pub unknown_reason: Option<String>,
}

impl PathLabel {
    fn unknown(reason: impl Into<String>) -> Self {
        Self {
            value: None,
            evidence: Vec::new(),
            unknown_reason: Some(reason.into()),
        }
    }

    fn observe(&mut self, value: bool, source: impl Into<String>, detail: impl Into<String>) {
        if self.evidence.is_empty() {
            self.value = Some(value);
            self.unknown_reason = None;
        } else if self.value != Some(value) {
            self.value = None;
            self.unknown_reason = Some("Conflicting classification declarations".into());
        }
        self.evidence.push(ClassificationEvidence {
            source: source.into(),
            detail: detail.into(),
        });
    }
}

#[derive(Debug, Serialize)]
pub struct PathClassification {
    pub path: String,
    pub tracked: PathLabel,
    pub ignored: PathLabel,
    pub generated: PathLabel,
    pub build_output: PathLabel,
    pub tracked_descendants: Option<usize>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PathClassifications {
    pub repository_root: String,
    pub labels_independent: bool,
    pub coherent_snapshot_verified: bool,
    pub paths: Vec<PathClassification>,
}

/// Exact index membership, ignore rules evaluated independently of the index,
/// and explicit provenance declarations. Absence of provenance is not false.
pub async fn classify_paths(
    cwd: PathBuf,
    paths: Vec<String>,
) -> Result<PathClassifications, String> {
    if paths.is_empty() || paths.len() > 64 {
        return Err("classify_paths requires 1-64 literal repository-root-relative paths".into());
    }
    let paths = paths
        .into_iter()
        .map(|path| {
            if path.len() > 4096 || path.split(['/', '\\']).count() > 64 {
                return Err(
                    "classification paths must be at most 4096 bytes and 64 components".into(),
                );
            }
            relative_git_path(&path)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || capture(&cwd, paths, &control))
        .await
        .map_err(|error| error.to_string())?
}

fn capture(
    cwd: &Path,
    paths: Vec<String>,
    control: &GitCommandControl,
) -> Result<PathClassifications, String> {
    let root = read_git(cwd, &["rev-parse", "--show-toplevel"], 32 * 1024, control)?;
    let root = root
        .strip_suffix('\n')
        .unwrap_or(&root)
        .trim_end_matches('\r');
    let root = PathBuf::from(root);
    let mut records = paths
        .iter()
        .map(|path| PathClassification {
            path: path.clone(),
            tracked: PathLabel::unknown("Index observation unavailable"),
            ignored: PathLabel::unknown("Ignore-rule observation unavailable"),
            generated: PathLabel::unknown(
                "No explicit generated declaration or recognized header observed",
            ),
            build_output: PathLabel::unknown(
                "No explicit build-output declaration or Cargo cache marker observed",
            ),
            tracked_descendants: None,
            errors: Vec::new(),
        })
        .collect::<Vec<_>>();
    let input = paths
        .iter()
        .flat_map(|path| path.bytes().chain(std::iter::once(0)))
        .collect::<Vec<_>>();
    let mut index_args = vec!["--literal-pathspecs", "ls-files", "--cached", "-z", "--"];
    index_args.extend(paths.iter().map(String::as_str));
    let index = command(&root, &index_args, None, false, control).and_then(|text| {
        nul_fields(&text).map(|fields| {
            fields
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        })
    });
    let ignored = command(
        &root,
        &[
            "check-ignore",
            "--no-index",
            "--verbose",
            "--non-matching",
            "-z",
            "--stdin",
        ],
        Some(&input),
        true,
        control,
    )
    .and_then(|text| parse_records(&text, 4, 3, paths.len()));
    let attributes = command(
        &root,
        &[
            "check-attr",
            "-z",
            "--stdin",
            "linguist-generated",
            "generated",
            "build-output",
        ],
        Some(&input),
        false,
        control,
    )
    .and_then(|text| parse_attributes(&text, paths.len()));
    for record in &mut records {
        check_control(control)?;
        match &index {
            Ok(index) => {
                let prefix = format!("{}/", record.path);
                record.tracked_descendants = Some(
                    index
                        .iter()
                        .filter(|name| name.starts_with(&prefix))
                        .count(),
                );
                record.tracked.observe(index.contains(&record.path), "git ls-files --cached -z",
                    "Exact path membership in the live index; directories themselves are not index entries");
            }
            Err(error) => record.errors.push(format!("tracked: {error}")),
        }
        match &ignored {
            Ok(ignored) => {
                if let Some(fields) = ignored.get(&record.path) {
                    let pattern = &fields[2];
                    record.ignored.observe(
                        !pattern.is_empty() && !pattern.starts_with('!'),
                        if fields[0].is_empty() {
                            "git check-ignore --no-index -v -z".into()
                        } else {
                            format!("{}:{}", fields[0], fields[1])
                        },
                        if pattern.is_empty() {
                            "No matching ignore rule (including Git global/info excludes)".into()
                        } else {
                            format!("Winning pattern: {pattern}; evaluated even for tracked paths")
                        },
                    );
                } else {
                    record
                        .errors
                        .push("ignored: Git did not return the requested path".into());
                }
            }
            Err(error) => record.errors.push(format!("ignored: {error}")),
        }
        match &attributes {
            Ok(attributes) => {
                if let Some(attributes) = attributes.get(&record.path) {
                    for (name, value) in attributes {
                        let label = if name == "build-output" {
                            &mut record.build_output
                        } else {
                            &mut record.generated
                        };
                        let declaration = match value.as_str() {
                            "set" | "true" => Some(true),
                            "unset" | "false" => Some(false),
                            _ => None,
                        };
                        if let Some(value) = declaration {
                            label.observe(
                                value,
                                "git check-attr -z",
                                format!(
                                    "{name}={}; effective Git attribute declaration",
                                    attributes[name]
                                ),
                            );
                        }
                    }
                } else {
                    record
                        .errors
                        .push("attributes: Git did not return the requested path".into());
                }
            }
            Err(error) => record.errors.push(format!("attributes: {error}")),
        }
        if let Err(error) = inspect_markers(&root, record, control) {
            record.errors.push(format!("markers: {error}"));
        }
    }
    Ok(PathClassifications {
        repository_root: root.to_string_lossy().into_owned(),
        labels_independent: true,
        coherent_snapshot_verified: false,
        paths: records,
    })
}

fn command(
    root: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    allow_no_match: bool,
    control: &GitCommandControl,
) -> Result<String, String> {
    let output = read_git_command_with_input(root, args, None, &[], CAPTURE_LIMIT, control, input)?;
    if (!output.status.success() && !(allow_no_match && output.status.code() == Some(1)))
        || !output.stderr.is_empty()
    {
        return Err(format!(
            "Git classification observation failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "Git classification requires lossless UTF-8 paths and rules".into())
}

fn nul_fields(text: &str) -> Result<Vec<&str>, String> {
    if !text.is_empty() && !text.ends_with('\0') {
        return Err("Incomplete NUL-framed Git response".into());
    }
    Ok(text.split_terminator('\0').collect())
}

fn parse_records(
    text: &str,
    width: usize,
    key: usize,
    count: usize,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let fields = nul_fields(text)?;
    if fields.len() != count * width {
        return Err("Incomplete Git classification records".into());
    }
    Ok(fields
        .chunks_exact(width)
        .map(|row| {
            (
                row[key].to_string(),
                row.iter().map(|field| (*field).to_string()).collect(),
            )
        })
        .collect())
}

fn parse_attributes(
    text: &str,
    count: usize,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let fields = nul_fields(text)?;
    if fields.len() != count * 9 {
        return Err("Incomplete Git attribute records".into());
    }
    let mut attributes = BTreeMap::<String, BTreeMap<String, String>>::new();
    for row in fields.chunks_exact(3) {
        if !matches!(row[1], "linguist-generated" | "generated" | "build-output") {
            return Err("Unexpected Git classification attribute".into());
        }
        attributes
            .entry(row[0].into())
            .or_default()
            .insert(row[1].into(), row[2].into());
    }
    Ok(attributes)
}

fn read_prefix(path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !metadata.is_file() || metadata.is_symlink() {
        return Ok(None);
    }
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(HEADER_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    // Header markers are ASCII. Non-UTF-8/binary prefixes never prove absence.
    Ok(std::str::from_utf8(&bytes)
        .ok()
        .filter(|text| !text.contains('\0'))
        .map(str::to_owned))
}

fn check_control(control: &GitCommandControl) -> Result<(), String> {
    control.check().map_err(|_| "Path classification cancelled or exceeded its 30-second deadline; no repository writes were requested".into())
}

fn inspect_markers(
    root: &Path,
    record: &mut PathClassification,
    control: &GitCommandControl,
) -> Result<(), String> {
    let mut current = root.to_path_buf();
    let mut ancestors = vec![current.clone()];
    for component in record.path.split('/') {
        check_control(control)?;
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err("Content markers are not followed through symlinks/junctions".into());
            }
            Ok(metadata) if metadata.is_dir() => ancestors.push(current.clone()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("{}: {error}", current.display())),
        }
    }
    // Use the complete requested path, including when an ancestor is absent.
    let path = root.join(&record.path);
    let header = match read_prefix(&path) {
        Ok(header) => header,
        Err(error) => {
            record.errors.push(format!("generated header: {error}"));
            None
        }
    };
    if let Some(header) = header {
        for (line, text) in header.lines().take(16).enumerate() {
            let text = text.trim_start_matches('\u{feff}').trim();
            let lower = text.to_ascii_lowercase();
            let body = ["//", "#", "/*", "*", "<!--"]
                .iter()
                .find_map(|prefix| lower.strip_prefix(prefix))
                .map(str::trim);
            let declaration = body.is_some_and(|body| {
                body == "@generated"
                    || body.starts_with("@generated ")
                    || body.starts_with("this file is @generated ")
                    || ([
                        "code generated ",
                        "generated code",
                        "this file was generated ",
                        "this file is generated ",
                        "automatically generated",
                        "this file is automatically generated",
                    ]
                    .iter()
                    .any(|prefix| body.starts_with(prefix))
                        && (body.contains("do not edit") || body.contains("do not modify")))
            });
            if declaration {
                record.generated.observe(
                    true,
                    format!("{}:{}", record.path, line + 1),
                    format!(
                        "Generated-header declaration: {}",
                        text.chars().take(240).collect::<String>()
                    ),
                );
                break;
            }
        }
    }
    for ancestor in ancestors.into_iter().rev() {
        check_control(control)?;
        let tag = ancestor.join("CACHEDIR.TAG");
        let text = match read_prefix(&tag) {
            Ok(text) => text,
            Err(error) => {
                record.errors.push(format!("build-output marker: {error}"));
                None
            }
        };
        if let Some(text) = text
            && text.starts_with("Signature: 8a477f597d28d172789f06886806bc55")
            && text.contains("cache directory tag created by cargo")
        {
            record.build_output.observe(
                true,
                tag.display().to_string(),
                "Cargo-created build/cache directory marker; not inferred from a directory name",
            );
            break;
        }
    }
    Ok(())
}
