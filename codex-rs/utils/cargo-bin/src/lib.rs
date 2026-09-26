use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CargoBinError {
    #[error("CARGO_BIN_EXE env var {key} resolved to {path:?}, which is not an existing file")]
    ResolvedPathIsNotAFile { key: String, path: PathBuf },
    #[error("CARGO_BIN_EXE env var {key} resolved to {path:?}, but an absolute path is required")]
    ResolvedPathIsRelative { key: String, path: PathBuf },
    #[error("could not locate binary {name:?}; tried env vars {env_keys:?}; {fallback}")]
    NotFound {
        name: String,
        env_keys: Vec<String>,
        fallback: String,
    },
    #[error(
        "binary {name:?} at {path:?} was built before its input {input:?} changed or was removed; \
         rebuild it or run the test through the repository test runner instead of using the stale build"
    )]
    StaleFallback {
        name: String,
        path: PathBuf,
        input: PathBuf,
    },
}

/// Returns an absolute path to a binary target built for the current Cargo test run.
pub fn cargo_bin(name: &str) -> Result<PathBuf, CargoBinError> {
    let env_keys = cargo_bin_env_keys(name);
    for key in &env_keys {
        if let Some(value) = std::env::var_os(key) {
            return resolve_bin_from_env(key, value);
        }
    }
    // Cargo puts integration tests in target/<profile>/deps and helper binaries
    // alongside that directory. assert_cmd's fallback now panics when Cargo did
    // not export the binary, but callers rely on this Result to try another helper.
    let fallback = std::env::current_exe().and_then(|mut path| {
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            Ok(path)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("binary does not exist at {}", path.display()),
            ))
        }
    });
    let path = fallback.map_err(|error| CargoBinError::NotFound {
        name: name.to_owned(),
        env_keys,
        fallback: error.to_string(),
    })?;
    // Nothing rebuilds a binary from another package before this test runs, so
    // the file found here may predate the sources it was built from.
    match stale_build_input(&path) {
        Some(input) => Err(CargoBinError::StaleFallback {
            name: name.to_owned(),
            path,
            input,
        }),
        None => Ok(path),
    }
}

/// Returns the first input recorded in the Cargo dep-info file beside `binary`
/// that is missing or newer than `binary`: the same comparison Cargo uses to
/// decide that the binary needs a rebuild. A binary without dep-info is not
/// Cargo's build output, so there is nothing to compare.
fn stale_build_input(binary: &Path) -> Option<PathBuf> {
    let built = std::fs::metadata(binary)
        .and_then(|metadata| metadata.modified())
        .ok()?;
    let dep_info = std::fs::read_to_string(binary.with_extension("d")).ok()?;
    dep_info_inputs(&dep_info).into_iter().find(|input| {
        match std::fs::metadata(input).and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified > built,
            // A recorded input that no longer exists also makes the build stale.
            Err(_) => true,
        }
    })
}

/// Parses the input list of Cargo's Makefile-style `<target>: <inputs>` rule,
/// which escapes spaces inside a path as `\ `.
fn dep_info_inputs(dep_info: &str) -> Vec<PathBuf> {
    let Some((_, inputs)) = dep_info
        .lines()
        .next()
        .and_then(|rule| rule.split_once(": "))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = inputs.trim_end().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    paths
}

fn cargo_bin_env_keys(name: &str) -> Vec<String> {
    let mut keys = Vec::with_capacity(2);
    keys.push(format!("CARGO_BIN_EXE_{name}"));

    // The repository's rust_test_runner exports both spellings for helper binaries.
    let underscore_name = name.replace('-', "_");
    if underscore_name != name {
        keys.push(format!("CARGO_BIN_EXE_{underscore_name}"));
    }

    keys
}

fn resolve_bin_from_env(key: &str, value: OsString) -> Result<PathBuf, CargoBinError> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(CargoBinError::ResolvedPathIsRelative {
            key: key.to_owned(),
            path,
        });
    }
    // Match the fallback's check: a directory cannot be launched as the helper.
    if path.is_file() {
        return Ok(path);
    }

    Err(CargoBinError::ResolvedPathIsNotAFile {
        key: key.to_owned(),
        path,
    })
}

/// Resolve a test resource relative to the consuming Cargo crate.
#[macro_export]
macro_rules! find_resource {
    ($resource:expr) => {{
        let resource = std::path::Path::new(&$resource);
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        Ok::<std::path::PathBuf, std::io::Error>(manifest_dir.join(resource))
    }};
}

fn resolve_cargo_resource(resource: &Path) -> io::Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir.join(resource))
}

pub fn repo_root() -> io::Result<PathBuf> {
    let mut root = resolve_cargo_resource(Path::new("repo_root.marker"))?;
    for _ in 0..4 {
        root = root
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "repo_root.marker did not have expected parent depth",
                )
            })?
            .to_path_buf();
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_returns_not_found_instead_of_panicking() {
        let name = "codex-test-helper-that-does-not-exist-8ba6b5a0";
        let error = cargo_bin(name).expect_err("missing helper should allow caller fallback");
        assert!(matches!(error, CargoBinError::NotFound { name: missing, .. } if missing == name));
    }

    #[test]
    fn dep_info_inputs_unescape_spaces_inside_paths() {
        let inputs = dep_info_inputs(
            "C:\\target\\debug\\helper.exe: C:\\src\\main.rs C:\\Program\\ Files\\lib.rs\r\n",
        );

        assert_eq!(
            inputs,
            vec![
                PathBuf::from(r"C:\src\main.rs"),
                PathBuf::from(r"C:\Program Files\lib.rs"),
            ]
        );
    }

    #[test]
    fn fallback_binary_is_stale_once_a_recorded_input_changes_or_disappears() -> std::io::Result<()>
    {
        fn set_modified(path: &Path, time: std::time::SystemTime) -> std::io::Result<()> {
            std::fs::File::options()
                .write(true)
                .open(path)?
                .set_modified(time)
        }
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("helper.exe");
        let source = dir.path().join("main.rs");
        std::fs::write(&binary, "binary")?;
        std::fs::write(&source, "fn main() {}")?;
        assert_eq!(
            stale_build_input(&binary),
            None,
            "a binary without dep-info has no recorded inputs"
        );
        let escape = |path: &Path| path.display().to_string().replace(' ', "\\ ");
        std::fs::write(
            dir.path().join("helper.d"),
            format!("{}: {}\n", escape(&binary), escape(&source)),
        )?;
        let built = std::time::SystemTime::now();
        let minute = std::time::Duration::from_secs(60);
        set_modified(&binary, built)?;

        set_modified(&source, built - minute)?;
        assert_eq!(stale_build_input(&binary), None);

        set_modified(&source, built + minute)?;
        assert_eq!(stale_build_input(&binary), Some(source.clone()));

        std::fs::remove_file(&source)?;
        assert_eq!(stale_build_input(&binary), Some(source));
        Ok(())
    }

    #[test]
    fn relative_environment_path_reports_the_absolute_path_requirement() {
        let error = resolve_bin_from_env("CARGO_BIN_EXE_example", OsString::from("Cargo.toml"))
            .expect_err("relative environment path");
        assert!(
            matches!(&error, CargoBinError::ResolvedPathIsRelative { path, .. } if path == Path::new("Cargo.toml"))
        );
        assert!(error.to_string().contains("an absolute path is required"));
    }

    #[test]
    fn environment_path_must_name_an_existing_file() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("helper.exe");
        for rejected in [dir.path().to_path_buf(), binary.clone()] {
            let error = resolve_bin_from_env("CARGO_BIN_EXE_example", rejected.clone().into())
                .expect_err("a directory or missing file is not a binary");
            assert!(
                matches!(&error, CargoBinError::ResolvedPathIsNotAFile { path, .. } if *path == rejected)
            );
        }
        std::fs::write(&binary, "binary")?;
        assert_eq!(
            resolve_bin_from_env("CARGO_BIN_EXE_example", binary.clone().into()).ok(),
            Some(binary)
        );
        Ok(())
    }
}
