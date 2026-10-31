use std::ffi::OsString;
use std::io;
use std::path::Component;
use std::path::Path;

use codex_utils_path_uri::PathUri;

use crate::ExecutorFileSystem;
use crate::protocol::LookupExecutableParams;
use crate::protocol::LookupExecutableResponse;

const MAX_CANDIDATES: usize = 4096;

/// Resolve files only: never start a shell, inspect shell profiles or launch the result.
/// Called on the executor so host PATH/PATHEXT cannot leak into remote lookups.
fn prepare_lookup(
    params: &LookupExecutableParams,
    max_candidates: usize,
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
    let mut truncated = false;
    let mut unresolved = None;
    'directories: for directory in directories {
        let base = cwd.as_path().join(&directory).join(program);
        for suffix in &suffixes {
            if candidates.len() == max_candidates {
                truncated = true;
                break 'directories;
            }
            let mut candidate = base.as_os_str().to_os_string();
            candidate.push(suffix);
            match PathUri::from_host_native_path(Path::new(&candidate)) {
                Ok(candidate) => candidates.push(candidate),
                // A drive-relative entry (`D:tools`, `D:`) resolves against a
                // process-private per-drive cwd, not the selected cwd. Like a failed
                // probe it stops the ordered search: earlier entries still decide.
                Err(error) => {
                    unresolved = Some(format!(
                        "unable to resolve PATH entry {}: {error}",
                        directory.display()
                    ));
                    break 'directories;
                }
            }
        }
    }
    let incomplete = unresolved.or_else(|| truncated.then(|| format!(
        "executable lookup exceeded {max_candidates} candidate probes"
    )));
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
    let plan = prepare_lookup(params, MAX_CANDIDATES)?;
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
    Ok(LookupExecutableResponse { path })
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
    let mut bytes_left = 4 * 1024 * 1024usize;
    for name in &params.names {
        let lookup = LookupExecutableParams { name: name.clone(), cwd: params.cwd.clone(), sandbox: None };
        match prepare_lookup(&lookup, MAX_CANDIDATES.min(probes_left)) {
            Err(error) => results.push(FileSystemLookupResult::from_result(Err(error))),
            Ok(mut plan) => {
                let mut retained = 0;
                for path in &plan.candidates {
                    if path.as_str().len() > bytes_left {
                        plan.incomplete = Some("executable batch exceeded 4 MiB of candidate path text".into());
                        break;
                    }
                    bytes_left -= path.as_str().len();
                    retained += 1;
                }
                plan.candidates.truncate(retained);
                probes_left -= retained;
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
