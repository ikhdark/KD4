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
pub(crate) async fn lookup_executable(
    filesystem: &dyn ExecutorFileSystem,
    params: &LookupExecutableParams,
) -> io::Result<LookupExecutableResponse> {
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
            if candidates.len() == MAX_CANDIDATES {
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
    // One backend operation: a sandboxed filesystem probes every candidate in a
    // single helper launch rather than one launch per candidate.
    let path = filesystem
        .first_regular_file(&candidates, params.sandbox.as_ref())
        .await
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{error}; availability is unknown"))
        })?;
    if path.is_none()
        && let Some(unresolved) = unresolved
    {
        return Err(io::Error::other(format!("{unresolved}; availability is unknown")));
    }
    if path.is_none() && truncated {
        return Err(io::Error::other(format!(
            "executable lookup exceeded {MAX_CANDIDATES} candidate probes; availability is unknown"
        )));
    }
    Ok(LookupExecutableResponse { path })
}
