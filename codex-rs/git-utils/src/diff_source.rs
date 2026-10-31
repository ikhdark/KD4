//! Source projections from the same bounded Git capture as its hunk coordinates.
use serde::Deserialize;
use serde::Serialize;

use crate::historical::valid_oid;
use crate::inspection::DiffFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffSourceRequest {
    #[serde(default = "default_context")]
    pub context_lines: usize,
    #[serde(default = "default_bytes")]
    pub max_bytes: usize,
    pub if_diff_sha256: Option<String>,
}

fn default_context() -> usize {
    3
}
fn default_bytes() -> usize {
    16 * 1024
}

impl DiffSourceRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.context_lines > 100 || self.max_bytes > 128 * 1024 {
            return Err(
                "source ranges require context_lines 0..100 and max_bytes 0..131072".into(),
            );
        }
        if self.if_diff_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("if_diff_sha256 must be a 64-hex complete diff identity".into());
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub struct DiffSourceRanges {
    pub coverage: &'static str,
    pub behavioral_review_complete: bool,
    pub coordinate_space: &'static str,
    pub context_lines: usize,
    pub files: Vec<SourceFile>,
    pub files_truncated: bool,
    pub returned_source_bytes: usize,
    pub ranges_complete: bool,
}

#[derive(Serialize)]
pub struct SourceFile {
    pub old: SourceSide,
    pub new: SourceSide,
    pub hunks: Vec<SourceHunk>,
    pub total_hunks: usize,
    pub ranges_complete: bool,
    pub unavailable_reason: Option<&'static str>,
}

#[derive(Serialize)]
pub struct SourceSide {
    pub path: String,
    pub mode: String,
    pub blob_oid: Option<String>,
    pub present: bool,
}

#[derive(Serialize)]
pub struct SourceHunk {
    pub old: SourceRange,
    pub new: SourceRange,
}

#[derive(Serialize)]
pub struct SourceRange {
    /// When line_count is zero, start_line is Git's insertion/deletion anchor,
    /// not a source line. It may be zero at the start of an empty/absent side.
    pub start_line: usize,
    pub line_count: usize,
    pub text: String,
}

fn side(path: &str, mode: &str, oid: &str) -> SourceSide {
    SourceSide {
        path: path.into(),
        mode: mode.into(),
        present: mode != "000000",
        blob_oid: (!oid.bytes().all(|byte| byte == b'0')).then(|| oid.into()),
    }
}

pub(crate) fn resolve(
    files: &[DiffFile],
    patch: &[u8],
    request: &DiffSourceRequest,
    max_files: usize,
) -> Result<DiffSourceRanges, String> {
    let mut blocks = Vec::new();
    let mut offset = 0;
    for line in patch.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            blocks.push(offset);
        }
        offset += line.len();
    }
    if blocks.len() != files.len() {
        return Err("Git source ranges require one patch block per raw file record".into());
    }
    blocks.push(patch.len());
    let mut remaining = request.max_bytes;
    let mut remaining_hunks = 1000usize;
    let mut selected = Vec::new();
    for (index, file) in files.iter().take(max_files).enumerate() {
        let mut source = SourceFile {
            old: side(&file.old_path, &file.old_mode, &file.old_oid),
            new: side(&file.new_path, &file.new_mode, &file.new_oid),
            hunks: Vec::new(),
            total_hunks: 0,
            ranges_complete: true,
            unavailable_reason: None,
        };
        let block = &patch[blocks[index]..blocks[index + 1]];
        // A working-tree raw OID can be zero. The full-index patch header
        // identifies Git's captured, normalized content without storing a blob.
        for line in block.split(|byte| *byte == b'\n') {
            if let Some(ids) = line.strip_prefix(b"index ") {
                let ids = std::str::from_utf8(ids).map_err(|_| "invalid Git index identities")?;
                let ids = ids
                    .split(' ')
                    .next()
                    .ok_or("missing Git index identities")?;
                let (old, new) = ids.split_once("..").ok_or("missing Git index identities")?;
                for (side, oid) in [(&mut source.old, old), (&mut source.new, new)] {
                    if !valid_oid(oid) || side.blob_oid.as_ref().is_some_and(|raw| raw != oid) {
                        return Err("Git raw/patch source identity mismatch".into());
                    }
                    side.blob_oid = (!oid.bytes().all(|byte| byte == b'0')).then(|| oid.into());
                }
                break;
            }
        }
        source.unavailable_reason = if file.binary {
            Some("binary_source")
        } else if [&file.old_mode, &file.new_mode]
            .iter()
            .any(|mode| !matches!(mode.as_str(), "000000" | "100644" | "100755"))
        {
            Some("not_regular_file")
        } else if std::str::from_utf8(block).is_err() {
            Some("source_not_utf8")
        } else {
            None
        };
        if source.unavailable_reason.is_some() {
            source.ranges_complete = false;
        } else {
            let text = std::str::from_utf8(block).map_err(|_| "source_not_utf8")?;
            let hunks = parse_hunks(text)?;
            source.total_hunks = hunks.len();
            for hunk in hunks {
                let bytes = hunk.old.text.len() + hunk.new.text.len();
                if bytes <= remaining && remaining_hunks > 0 {
                    remaining -= bytes;
                    remaining_hunks -= 1;
                    source.hunks.push(hunk);
                } else {
                    source.ranges_complete = false;
                }
            }
            for side in [&source.old, &source.new] {
                if side.present && side.blob_oid.is_none() {
                    return Err("Git did not identify a captured source side".into());
                }
            }
        }
        selected.push(source);
    }
    Ok(DiffSourceRanges {
        coverage: "changed_lines_with_context",
        behavioral_review_complete: false,
        coordinate_space: "git_blob_lines_after_builtin_normalization",
        context_lines: request.context_lines,
        files_truncated: files.len() > max_files,
        ranges_complete: files.len() <= max_files
            && selected.iter().all(|file| file.ranges_complete),
        files: selected,
        returned_source_bytes: request.max_bytes - remaining,
    })
}

fn coordinates(value: &str, prefix: char) -> Result<SourceRange, String> {
    let value = value.strip_prefix(prefix).ok_or("invalid Git hunk side")?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start_line = start
        .parse::<usize>()
        .map_err(|_| "invalid Git hunk start")?;
    let line_count = count
        .parse::<usize>()
        .map_err(|_| "invalid Git hunk count")?;
    if (start_line == 0 && line_count != 0) || start_line.checked_add(line_count).is_none() {
        return Err("invalid Git hunk coordinates".into());
    }
    Ok(SourceRange {
        start_line,
        line_count,
        text: String::new(),
    })
}

fn parse_hunks(block: &str) -> Result<Vec<SourceHunk>, String> {
    let mut lines = block.split_inclusive('\n').peekable();
    let mut hunks = Vec::new();
    while let Some(line) = lines.next() {
        let Some(header) = line.strip_prefix("@@ ") else {
            continue;
        };
        let mut header = header.trim_end_matches(['\r', '\n']).split(' ');
        let mut old = coordinates(header.next().ok_or("missing old hunk")?, '-')?;
        let mut new = coordinates(header.next().ok_or("missing new hunk")?, '+')?;
        if header.next() != Some("@@") {
            return Err("invalid Git hunk header".into());
        }
        let (mut old_count, mut new_count) = (0, 0);
        while old_count < old.line_count || new_count < new.line_count {
            let line = lines.next().ok_or("truncated Git source hunk")?;
            // diff.suppressBlankEmpty may omit the space on empty context
            // lines. These still denote one LF on each source side.
            let (indicator, text) = if line == "\n" {
                (" ", line)
            } else {
                line.split_at(1)
            };
            match indicator {
                " " => {
                    old.text.push_str(text);
                    new.text.push_str(text);
                    old_count += 1;
                    new_count += 1;
                }
                "-" => {
                    old.text.push_str(text);
                    old_count += 1;
                }
                "+" => {
                    new.text.push_str(text);
                    new_count += 1;
                }
                _ => return Err("invalid Git source hunk line".into()),
            }
            if lines
                .peek()
                .is_some_and(|line| line.starts_with("\\ "))
            {
                lines.next();
                if indicator != "+" && old.text.ends_with('\n') {
                    old.text.pop();
                }
                if indicator != "-" && new.text.ends_with('\n') {
                    new.text.pop();
                }
            }
            if old_count > old.line_count || new_count > new.line_count {
                return Err("Git source hunk count mismatch".into());
            }
        }
        hunks.push(SourceHunk { old, new });
    }
    Ok(hunks)
}
