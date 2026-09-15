use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CargoBinError {
    #[error("CARGO_BIN_EXE env var {key} resolved to {path:?}, but it does not exist")]
    ResolvedPathDoesNotExist { key: String, path: PathBuf },
    #[error("CARGO_BIN_EXE env var {key} resolved to {path:?}, but an absolute path is required")]
    ResolvedPathIsRelative { key: String, path: PathBuf },
    #[error("could not locate binary {name:?}; tried env vars {env_keys:?}; {fallback}")]
    NotFound {
        name: String,
        env_keys: Vec<String>,
        fallback: String,
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
    fallback.map_err(|error| CargoBinError::NotFound {
        name: name.to_owned(),
        env_keys,
        fallback: error.to_string(),
    })
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
    if path.exists() {
        return Ok(path);
    }

    Err(CargoBinError::ResolvedPathDoesNotExist {
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
    fn relative_environment_path_reports_the_absolute_path_requirement() {
        let error = resolve_bin_from_env("CARGO_BIN_EXE_example", OsString::from("Cargo.toml"))
            .expect_err("relative environment path");
        assert!(
            matches!(&error, CargoBinError::ResolvedPathIsRelative { path, .. } if path == Path::new("Cargo.toml"))
        );
        assert!(error.to_string().contains("an absolute path is required"));
    }
}
