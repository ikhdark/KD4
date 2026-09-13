use crate::plugin_bundle_archive::unpack_plugin_bundle_tar_gz;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

const NPM_PLUGIN_SOURCE_STAGING_DIR: &str = "plugins/.marketplace-plugin-source-staging";
const NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES: u64 = 50 * 1024 * 1024;
const NPM_PLUGIN_SOURCE_MAX_EXTRACTED_BYTES: u64 = 250 * 1024 * 1024;
const NPM_PACKAGE_ARCHIVE_ROOT: &str = "package";

pub(crate) fn materialize_npm_plugin_source(
    codex_home: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
) -> Result<(AbsolutePathBuf, TempDir), String> {
    materialize_npm_plugin_source_with_command(
        codex_home,
        package,
        version,
        registry,
        OsStr::new(npm_command()),
    )
}

fn materialize_npm_plugin_source_with_command(
    codex_home: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    npm_command: &OsStr,
) -> Result<(AbsolutePathBuf, TempDir), String> {
    let staging_root = codex_home.join(NPM_PLUGIN_SOURCE_STAGING_DIR);
    fs::create_dir_all(&staging_root).map_err(|err| {
        format!(
            "failed to create marketplace plugin source staging directory {}: {err}",
            staging_root.display()
        )
    })?;
    let tempdir = tempfile::Builder::new()
        .prefix("marketplace-plugin-source-")
        .tempdir_in(&staging_root)
        .map_err(|err| {
            format!(
                "failed to create marketplace plugin source staging directory in {}: {err}",
                staging_root.display()
            )
        })?;

    pack_npm_package(tempdir.path(), package, version, registry, npm_command)?;
    let archive_path = find_npm_package_archive(tempdir.path())?;
    let archive_bytes = read_npm_package_archive(&archive_path)?;

    let extraction_root = tempdir.path().join("extracted");
    unpack_plugin_bundle_tar_gz(
        &archive_bytes,
        &extraction_root,
        NPM_PLUGIN_SOURCE_MAX_EXTRACTED_BYTES,
    )
    .map_err(|err| format!("failed to extract npm plugin package: {err}"))?;
    let plugin_root = extraction_root.join(NPM_PACKAGE_ARCHIVE_ROOT);
    if !plugin_root.is_dir() {
        return Err(format!(
            "npm pack completed without creating plugin package directory {}",
            plugin_root.display()
        ));
    }
    validate_npm_package_metadata(&plugin_root, package)?;
    let plugin_root = AbsolutePathBuf::try_from(plugin_root)
        .map_err(|err| format!("failed to resolve materialized plugin source path: {err}"))?;
    Ok((plugin_root, tempdir))
}

fn pack_npm_package(
    destination: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    npm_command: &OsStr,
) -> Result<(), String> {
    let package_spec = version.map_or_else(
        || package.to_string(),
        |version| format!("{package}@{version}"),
    );
    let mut command = Command::new(npm_command);
    command
        .current_dir(destination)
        .arg("pack")
        .arg("--ignore-scripts")
        .arg("--pack-destination")
        .arg(destination);
    if let Some(registry) = registry {
        command.arg("--registry").arg(registry);
    }
    command.arg("--").arg(package_spec);

    let output = command
        .output()
        .map_err(|err| format!("failed to run npm pack: {err}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "npm pack failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn find_npm_package_archive(destination: &Path) -> Result<PathBuf, String> {
    let entries = fs::read_dir(destination)
        .map_err(|err| format!("failed to read npm pack destination: {err}"))?;
    find_npm_package_archive_in(entries)
}

fn find_npm_package_archive_in(
    entries: impl IntoIterator<Item = io::Result<fs::DirEntry>>,
) -> Result<PathBuf, String> {
    let mut archives = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("failed to read npm pack destination entry: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("failed to inspect npm pack destination entry: {err}"))?;
        let path = entry.path();
        if file_type.is_file() && path.extension() == Some(OsStr::new("tgz")) {
            archives.push(path);
        }
    }
    if archives.len() != 1 {
        return Err(format!(
            "npm pack completed with {} package archives; expected exactly one",
            archives.len()
        ));
    }
    Ok(archives.remove(0))
}

fn read_npm_package_archive(archive_path: &Path) -> Result<Vec<u8>, String> {
    let archive = fs::File::open(archive_path)
        .map_err(|err| format!("failed to open npm package archive: {err}"))?;
    let archive_size = archive
        .metadata()
        .map_err(|err| format!("failed to inspect npm package archive: {err}"))?
        .len();
    if archive_size > NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES {
        return Err(format!(
            "npm package archive is {archive_size} bytes, exceeding maximum size of {NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES} bytes"
        ));
    }
    read_npm_package_archive_bytes(archive)
}

fn read_npm_package_archive_bytes(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("failed to read npm package archive: {err}"))?;
    if bytes.len() as u64 > NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES {
        return Err(format!(
            "npm package archive exceeds maximum size of {NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES} bytes"
        ));
    }
    Ok(bytes)
}

fn validate_npm_package_metadata(plugin_root: &Path, package: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct NpmPackageMetadata {
        name: String,
    }

    let package_json_path = plugin_root.join("package.json");
    let package_json = fs::read_to_string(&package_json_path).map_err(|err| {
        format!(
            "failed to read npm plugin package metadata {}: {err}",
            package_json_path.display()
        )
    })?;
    let metadata: NpmPackageMetadata = serde_json::from_str(&package_json).map_err(|err| {
        format!(
            "failed to parse npm plugin package metadata {}: {err}",
            package_json_path.display()
        )
    })?;
    if metadata.name != package {
        return Err(format!(
            "npm plugin package name '{}' does not match requested package '{package}'",
            metadata.name
        ));
    }
    Ok(())
}

fn npm_command() -> &'static str {
    "npm.cmd"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn archive_inventory_rejects_an_error_after_a_discovered_archive() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.tgz"), b"archive").unwrap();
        let archive = fs::read_dir(directory.path()).unwrap().next().unwrap();
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "incomplete inventory");
        let result = find_npm_package_archive_in([archive, Err(error)]);
        assert!(result.unwrap_err().contains("incomplete inventory"));
    }

    #[test]
    fn archive_reader_rejects_growth_and_stops_after_the_size_sentinel() {
        let mut reader = Cursor::new(vec![0; NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES as usize + 32]);
        let result = read_npm_package_archive_bytes(&mut reader);
        assert!(result.unwrap_err().contains("exceeds maximum size"));
        assert_eq!(reader.position(), NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES + 1);
        assert_eq!(
            read_npm_package_archive_bytes(Cursor::new(b"archive")).unwrap(),
            b"archive"
        );
    }

    #[cfg(windows)]
    fn fake_npm(directory: &Path, package_name: &str, extra_archive: bool) -> PathBuf {
        let archive_path = directory.join("fixture.tgz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let metadata = serde_json::json!({ "name": package_name }).to_string();
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "package/package.json", metadata.as_bytes())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        let command = directory.join("npm.cmd");
        let extra = if extra_archive {
            "copy /y package.tgz other.tgz >nul\r\n"
        } else {
            ""
        };
        fs::write(
            &command,
            format!(
                "@echo off\r\ncopy /y \"{}\" package.tgz >nul\r\n{extra}exit /b 0\r\n",
                archive_path.display()
            ),
        )
        .unwrap();
        command
    }

    #[cfg(windows)]
    #[test]
    fn npm_materialization_validates_package_and_keeps_staging_until_drop() {
        let directory = tempfile::tempdir().unwrap();
        let command = fake_npm(directory.path(), "@test/plugin", false);
        let (plugin, staging) = materialize_npm_plugin_source_with_command(
            directory.path(),
            "@test/plugin",
            Some("1.0.0"),
            None,
            command.as_os_str(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(plugin.as_path().join("package.json")).unwrap(),
            r#"{"name":"@test/plugin"}"#
        );
        assert!(plugin.as_path().starts_with(staging.path()));
        drop(staging);
        assert!(!plugin.as_path().exists());
    }

    #[cfg(windows)]
    #[test]
    fn npm_materialization_rejects_invalid_inventory_metadata_and_size_without_staging() {
        for (package_name, extra_archive, oversized, expected_error) in [
            (
                "wrong-package",
                false,
                false,
                "does not match requested package",
            ),
            ("@test/plugin", true, false, "2 package archives"),
            ("@test/plugin", false, true, "exceeding maximum size"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let command = fake_npm(directory.path(), package_name, extra_archive);
            if oversized {
                fs::OpenOptions::new()
                    .write(true)
                    .open(directory.path().join("fixture.tgz"))
                    .unwrap()
                    .set_len(NPM_PLUGIN_SOURCE_MAX_ARCHIVE_BYTES + 1)
                    .unwrap();
            }
            let error = materialize_npm_plugin_source_with_command(
                directory.path(),
                "@test/plugin",
                None,
                None,
                command.as_os_str(),
            )
            .unwrap_err();
            assert!(error.contains(expected_error), "{error}");
            assert_eq!(
                fs::read_dir(directory.path().join(NPM_PLUGIN_SOURCE_STAGING_DIR))
                    .unwrap()
                    .count(),
                0,
                "rejected materialization must not retain extracted or staged files"
            );
        }
    }
}
