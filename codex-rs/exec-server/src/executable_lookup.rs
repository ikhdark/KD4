use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::Component;
use std::path::Path;

use codex_utils_path_uri::PathUri;

use crate::ExecutorFileSystem;
use crate::protocol::LookupExecutableParams;
use crate::protocol::LookupExecutableResponse;

const MAX_CANDIDATES: usize = 4096;
const MAX_CANDIDATE_BYTES: usize = 4 * 1024 * 1024;

/// Resolve files only: never start a shell, inspect shell profiles or launch the result.
/// Called on the executor so host PATH/PATHEXT cannot leak into remote lookups.
fn prepare_lookup(
    params: &LookupExecutableParams,
    max_candidates: usize,
    bytes_left: &mut usize,
    byte_limit_message: &str,
) -> io::Result<LookupPlan> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    let name = &params.name;
    if name.is_empty() || name.len() > 4096 || name.contains(['\0', '*', '?', '"']) {
        return Err(invalid(
            "name must be a literal executable name or path, 1-4096 UTF-8 bytes",
        ));
    }
    let cwd = params
        .cwd
        .to_abs_path()
        .map_err(|error| invalid(&error.to_string()))?;
    let program = Path::new(name);
    if program.has_root() && !program.is_absolute()
        || !program.is_absolute()
            && matches!(program.components().next(), Some(Component::Prefix(_)))
    {
        return Err(invalid(
            "drive-relative and rooted-without-drive executable paths are ambiguous; use an absolute or cwd-relative path",
        ));
    }
    let pathext =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    let pathext = pathext
        .to_str()
        .ok_or_else(|| invalid("PATHEXT is not valid Unicode"))?;
    let mut extensions = Vec::new();
    for extension in pathext.split(';').filter(|extension| !extension.is_empty()) {
        if !extension.starts_with('.')
            || extension.len() > 256
            || extension.contains(['/', '\\', ':', '*', '?', '"', '\0'])
        {
            return Err(invalid("PATHEXT contains an invalid extension"));
        }
        if !extensions
            .iter()
            .any(|previous: &&str| previous.eq_ignore_ascii_case(extension))
        {
            extensions.push(extension);
        }
    }
    // A PATHEXT extension names the file exactly (git.exe, as in cmd and
    // PowerShell). Any other dot may belong to the name (python3.12,
    // gcc-13.2.0): like cmd, try the exact file first, then each PATHEXT suffix.
    // An exact non-PATHEXT match reports existence only; CreateProcess may
    // refuse it and cmd may open it through a file association instead.
    let extension = program.extension();
    let executable_extension = extension.is_some_and(|extension| {
        extensions.iter().any(|known| {
            known
                .strip_prefix('.')
                .is_some_and(|known| extension.eq_ignore_ascii_case(known))
        })
    });
    let suffixes = if executable_extension {
        vec![""]
    } else if extension.is_some() {
        std::iter::once("").chain(extensions).collect()
    } else {
        extensions
    };
    // An empty entry (such as a trailing `;`) is not an implicit cwd search.
    let directories = if program.is_absolute() || program.components().count() > 1 {
        vec![cwd.as_path().to_path_buf()]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|directory| !directory.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut candidates = Vec::new();
    let mut seen = params.all_matches.then(HashSet::new);
    let mut candidate_count = 0;
    let mut incomplete = None;
    'directories: for directory in directories {
        let base = cwd.as_path().join(&directory).join(program);
        for suffix in &suffixes {
            if candidate_count == max_candidates {
                incomplete = Some(format!(
                    "executable lookup exceeded {max_candidates} candidate probes"
                ));
                break 'directories;
            }
            let mut candidate = base.as_os_str().to_os_string();
            candidate.push(suffix);
            match PathUri::from_host_native_path(Path::new(&candidate)) {
                Ok(candidate) => {
                    // Census deduplication does not change the candidate count limit.
                    candidate_count += 1;
                    let candidate_bytes = candidate.as_str().len();
                    if candidate_bytes > *bytes_left {
                        if seen.as_ref().is_some_and(|seen| seen.contains(&candidate)) {
                            continue;
                        }
                        incomplete = Some(byte_limit_message.into());
                        break 'directories;
                    }
                    if seen
                        .as_mut()
                        .is_some_and(|seen| !seen.insert(candidate.clone()))
                    {
                        continue;
                    }
                    *bytes_left -= candidate_bytes;
                    candidates.push(candidate);
                }
                // A drive-relative entry (`D:tools`, `D:`) resolves against a
                // process-private per-drive cwd, not the selected cwd. Like a failed
                // probe it stops the ordered search: earlier entries still decide.
                Err(error) => {
                    incomplete = Some(format!(
                        "unable to resolve PATH entry {}: {error}",
                        directory.display()
                    ));
                    break 'directories;
                }
            }
        }
    }
    Ok(LookupPlan { candidates, incomplete })
}

struct LookupPlan {
    candidates: Vec<PathUri>,
    incomplete: Option<String>,
}

pub(crate) async fn lookup_executable(
    filesystem: &dyn ExecutorFileSystem,
    params: &LookupExecutableParams,
) -> io::Result<LookupExecutableResponse> {
    let mut bytes_left = MAX_CANDIDATE_BYTES;
    let plan = prepare_lookup(
        params,
        MAX_CANDIDATES,
        &mut bytes_left,
        "candidate paths exceed 4 MiB",
    )?;
    if params.all_matches {
        return lookup_all(filesystem, params, plan).await;
    }
    let path = filesystem
        .first_regular_file(&plan.candidates, params.sandbox.as_ref())
        .await
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{error}; availability is unknown"))
        })?;
    if path.is_none()
        && let Some(unresolved) = plan.incomplete
    {
        return Err(io::Error::other(format!("{unresolved}; availability is unknown")));
    }
    Ok(LookupExecutableResponse { path, matches: None })
}

async fn lookup_all(
    filesystem: &dyn ExecutorFileSystem,
    params: &LookupExecutableParams,
    plan: LookupPlan,
) -> io::Result<LookupExecutableResponse> {
    use codex_file_system::{FileSystemLookup, FileSystemLookupResult};

    let unknown = |error: &str| io::Error::other(format!("{error}; executable census is incomplete"));
    if let Some(error) = plan.incomplete {
        return Err(unknown(&error));
    }
    let requests: Vec<_> = plan
        .candidates
        .into_iter()
        .map(|path| FileSystemLookup::FirstRegularFile {
            candidates: vec![path],
        })
        .collect();
    let mut matches = Vec::new();
    let mut match_bytes = 0usize;
    for batch in requests.chunks(32) {
        let observed = filesystem
            .lookup_batch(batch, params.sandbox.as_ref())
            .await
            .map_err(|error| unknown(&error.to_string()))?;
        if observed.len() != batch.len() {
            return Err(unknown("lookup batch result count mismatch"));
        }
        for outcome in observed {
            match outcome {
                FileSystemLookupResult::Resolved { path } => {
                    match_bytes += path.as_str().len();
                    if matches.len() == 64 || match_bytes > 32 * 1024 {
                        return Err(unknown("matches exceed the 64 path / 32 KiB result bound"));
                    }
                    matches.push(path);
                }
                FileSystemLookupResult::Missing => {}
                FileSystemLookupResult::Inaccessible { error }
                | FileSystemLookupResult::Unsupported { error }
                | FileSystemLookupResult::Invalid { error }
                | FileSystemLookupResult::Unknown { error } => return Err(unknown(&error)),
            }
        }
    }
    Ok(LookupExecutableResponse {
        path: matches.first().cloned(),
        matches: Some(matches),
    })
}

pub(crate) async fn lookup_executables(
    filesystem: &dyn ExecutorFileSystem,
    params: &crate::protocol::LookupExecutablesParams,
) -> io::Result<Vec<codex_file_system::FileSystemLookupResult>> {
    use codex_file_system::FileSystemLookup;
    use codex_file_system::FileSystemLookupResult;
    if params.names.is_empty() || params.names.len() > 32 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected 1-32 executable names"));
    }
    let mut results = Vec::with_capacity(params.names.len());
    let mut requests = Vec::new();
    let mut pending = Vec::new();
    let mut probes_left = 16_384usize;
    let mut bytes_left = MAX_CANDIDATE_BYTES;
    for name in &params.names {
        let lookup = LookupExecutableParams { name: name.clone(), all_matches: false, cwd: params.cwd.clone(), sandbox: None };
        match prepare_lookup(
            &lookup,
            MAX_CANDIDATES.min(probes_left),
            &mut bytes_left,
            "executable batch exceeded 4 MiB of candidate path text",
        ) {
            Err(error) => results.push(FileSystemLookupResult::from_result(Err(error))),
            Ok(plan) => {
                probes_left -= plan.candidates.len();
                pending.push((results.len(), plan.incomplete));
                requests.push(FileSystemLookup::FirstRegularFile { candidates: plan.candidates });
                results.push(FileSystemLookupResult::Unknown { error: "lookup not completed".into() });
            }
        }
    }
    if !requests.is_empty() {
        let observed = match filesystem.lookup_batch(&requests, params.sandbox.as_ref()).await {
            Ok(observed) if observed.len() == requests.len() => observed,
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "lookup batch result count mismatch")),
            // A helper/transport failure (even NotFound) says nothing about
            // any target's existence. Only per-probe outcomes classify paths.
            Err(error) => vec![FileSystemLookupResult::Unknown { error: error.to_string() }; requests.len()],
        };
        for ((index, incomplete), outcome) in pending.into_iter().zip(observed) {
            results[index] = match (outcome, incomplete) {
                (FileSystemLookupResult::Missing, Some(error)) => FileSystemLookupResult::Unknown { error },
                (outcome, _) => outcome,
            };
        }
    }
    Ok(results)
}
