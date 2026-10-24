use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;

use crate::logging::log_note;
use crate::sandbox_bin_dir;

pub(crate) const BIN_DIRNAME: &str = "bin";
pub(crate) const RESOURCES_DIRNAME: &str = "codex-resources";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum HelperExecutable {
    CommandRunner,
}

impl HelperExecutable {
    fn file_name(self) -> &'static str {
        match self {
            Self::CommandRunner => "codex-command-runner.exe",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CommandRunner => "command-runner",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyOutcome {
    Reused,
    ReCopied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperMaterializationStatus {
    Reused,
    ReCopied,
    SourceFallback,
}


pub(crate) fn helper_bin_dir(codex_home: &Path) -> PathBuf {
    sandbox_bin_dir(codex_home)
}

pub(crate) fn legacy_lookup(kind: HelperExecutable) -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(candidate) = bundled_executable_path_for_exe(&exe, kind.file_name())
    {
        if candidate.is_absolute() && candidate.is_file() { return Ok(candidate); }
    }
    Err(anyhow!("packaged {} helper not found; refusing ambient executable lookup", kind.label()))
}

pub(crate) fn resolve_helper_for_launch(
    kind: HelperExecutable,
    codex_home: &Path,
    log_dir: Option<&Path>,
) -> Result<PathBuf> {
    match copy_helper_if_needed(kind, codex_home, log_dir) {
        Ok(path) => {
            log_note(
                &format!(
                    "helper launch resolution: using copied {} path {}",
                    kind.label(),
                    path.display()
                ),
                log_dir,
            );
            Ok(path)
        }
        Err(err) => {
            let fallback = legacy_lookup(kind)?;
            log_note(
                &format!(
                    "helper copy failed for {}: {err:#}; falling back to legacy path {}",
                    kind.label(),
                    fallback.display()
                ),
                log_dir,
            );
            Ok(fallback)
        }
    }
}

pub fn resolve_current_exe_for_launch(codex_home: &Path, fallback_executable: &str) -> PathBuf {
    resolve_current_exe_for_launch_with_status(codex_home, fallback_executable).0
}

pub fn resolve_current_exe_for_launch_with_status(
    codex_home: &Path,
    fallback_executable: &str,
) -> (PathBuf, HelperMaterializationStatus) {
    let source = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            return (
                PathBuf::from(fallback_executable),
                HelperMaterializationStatus::SourceFallback,
            );
        }
    };
    resolve_exe_for_launch_with_status(&source, codex_home)
}

pub fn resolve_exe_for_launch(source: &Path, codex_home: &Path) -> PathBuf {
    resolve_exe_for_launch_with_status(source, codex_home).0
}

pub fn resolve_exe_for_launch_with_status(
    source: &Path,
    codex_home: &Path,
) -> (PathBuf, HelperMaterializationStatus) {
    let Some(file_name) = source.file_name() else {
        return (
            source.to_path_buf(),
            HelperMaterializationStatus::SourceFallback,
        );
    };
    let digest = match content_digest(source) {
        Ok(digest) => digest,
        Err(_) => return (source.to_path_buf(), HelperMaterializationStatus::SourceFallback),
    };
    let mut name = source.file_stem().unwrap_or(file_name).to_os_string();
    name.push(format!("-{digest}"));
    if let Some(extension) = source.extension() { name.push("."); name.push(extension); }
    let destination = helper_bin_dir(codex_home).join(name);
    match copy_from_source_with_digest(source, &destination, &digest) {
        Ok(CopyOutcome::Reused) => (destination, HelperMaterializationStatus::Reused),
        Ok(CopyOutcome::ReCopied) => (destination, HelperMaterializationStatus::ReCopied),
        Err(err) => {
            let sandbox_log_dir = crate::sandbox_dir(codex_home);
            log_note(
                &format!(
                    "helper copy failed for executable: {err:#}; falling back to legacy path {}",
                    source.display()
                ),
                Some(&sandbox_log_dir),
            );
            (
                source.to_path_buf(),
                HelperMaterializationStatus::SourceFallback,
            )
        }
    }
}

pub(crate) fn copy_helper_if_needed(
    kind: HelperExecutable,
    codex_home: &Path,
    log_dir: Option<&Path>,
) -> Result<PathBuf> {
    let source = sibling_source_path(kind)?;
    let (destination, digest) = helper_destination_for_source(kind, codex_home, &source)?;
    log_note(
        &format!(
            "helper copy: validating {} source={} destination={}",
            kind.label(),
            source.display(),
            destination.display()
        ),
        log_dir,
    );
    let outcome = copy_from_source_with_digest(&source, &destination, &digest)?;
    let action = match outcome {
        CopyOutcome::Reused => "reused",
        CopyOutcome::ReCopied => "recopied",
    };
    log_note(
        &format!(
            "helper copy: {} {} source={} destination={}",
            action,
            kind.label(),
            source.display(),
            destination.display()
        ),
        log_dir,
    );
    Ok(destination)
}



fn sibling_source_path(kind: HelperExecutable) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve current executable for helper lookup")?;
    bundled_executable_path_for_exe(&exe, kind.file_name()).ok_or_else(|| {
        anyhow!(
            "helper not found next to current executable or under {RESOURCES_DIRNAME}: {}",
            exe.display()
        )
    })
}

pub(crate) fn bundled_executable_path_for_exe(exe: &Path, file_name: &str) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let direct_candidate = dir.join(file_name);
    if direct_candidate.is_file() {
        return Some(direct_candidate);
    }

    if dir.file_name() == Some(OsStr::new(BIN_DIRNAME))
        && let Some(package_dir) = dir.parent()
    {
        let package_resource_candidate = package_dir.join(RESOURCES_DIRNAME).join(file_name);
        if package_resource_candidate.is_file() {
            return Some(package_resource_candidate);
        }
    }

    let resource_candidate = dir.join(RESOURCES_DIRNAME).join(file_name);
    resource_candidate.is_file().then_some(resource_candidate)
}

fn helper_destination_for_source(
    kind: HelperExecutable,
    codex_home: &Path,
    source: &Path,
) -> Result<(PathBuf, String)> {
    let suffix = helper_version_suffix(source)?;
    let file_name = materialized_file_name(kind, &suffix);
    let digest = suffix.rsplit_once('-').context("missing helper content identity")?.1.to_owned();
    Ok((helper_bin_dir(codex_home).join(file_name), digest))
}

fn materialized_file_name(kind: HelperExecutable, suffix: &str) -> String {
    let source_name = kind.file_name();
    let path = Path::new(source_name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(source_name);
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    format!("{stem}-{suffix}{extension}")
}

fn helper_version_suffix(source: &Path) -> Result<String> {
    Ok(format!("{}-{}", env!("CARGO_PKG_VERSION"), content_digest(source)?))
}


#[cfg(test)]
fn copy_from_source_if_needed(source: &Path, destination: &Path) -> Result<CopyOutcome> {
    let expected = content_digest(source)?;
    copy_from_source_with_digest(source, destination, &expected)
}

fn copy_from_source_with_digest(source: &Path, destination: &Path, expected: &str) -> Result<CopyOutcome> {
    if destination_matches_digest(destination, expected)? {
        return Ok(CopyOutcome::Reused);
    }

    let destination_dir = destination.parent().ok_or_else(|| {
        anyhow!(
            "helper destination has no parent: {}",
            destination.display()
        )
    })?;
    fs::create_dir_all(destination_dir).with_context(|| {
        format!(
            "create helper destination directory {}",
            destination_dir.display()
        )
    })?;

    let temp_path = NamedTempFile::new_in(destination_dir)
        .with_context(|| {
            format!(
                "create temporary helper file in {}",
                destination_dir.display()
            )
        })?
        .into_temp_path();
    let temp_path_buf = temp_path.to_path_buf();

    let mut source_file = fs::File::open(source)
        .with_context(|| format!("open helper source for read {}", source.display()))?;
    let mut temp_file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temp_path_buf)
        .with_context(|| format!("open temporary helper file {}", temp_path_buf.display()))?;

    // Write into a temp file created inside `.sandbox-bin` so the copied helper keeps the
    // destination directory's inherited ACLs instead of reusing the source file's descriptor.
    std::io::copy(&mut source_file, &mut temp_file).with_context(|| {
        format!(
            "copy helper from {} to {}",
            source.display(),
            temp_path_buf.display()
        )
    })?;
    temp_file
        .flush()
        .with_context(|| format!("flush temporary helper file {}", temp_path_buf.display()))?;
    drop(temp_file);

    anyhow::ensure!(content_digest(&temp_path_buf)? == expected, "helper source changed while materializing {}", source.display());

    match replace_helper_file(&temp_path_buf, destination) {
        Ok(()) => Ok(CopyOutcome::ReCopied),
        Err(rename_err) => {
            if destination_matches_digest(destination, expected)? {
                Ok(CopyOutcome::Reused)
            } else {
                Err(rename_err).with_context(|| {
                    format!(
                        "rename helper temp file {} to {}",
                        temp_path_buf.display(),
                        destination.display()
                    )
                })
            }
        }
    }
}

fn replace_helper_file(temp_path: &Path, destination: &Path) -> std::io::Result<()> {
    // Replace in one operation so a failed rename leaves the previous helper intact.
    fs::rename(temp_path, destination)
}

#[cfg(test)]
fn destination_is_fresh(source: &Path, destination: &Path) -> Result<bool> {
    destination_matches_digest(destination, &content_digest(source)?)
}

fn destination_matches_digest(destination: &Path, expected: &str) -> Result<bool> {
    match content_digest(destination) {
        Ok(digest) => Ok(digest == expected),
        Err(err) if err.downcast_ref::<std::io::Error>().is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => Err(err),
    }
}

fn content_digest(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).with_context(|| format!("open helper image {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 { break; }
        hash.update(&bytes[..count]);
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::BIN_DIRNAME;
    use super::CopyOutcome;
    use super::HelperExecutable;
    use super::HelperMaterializationStatus;
    use super::RESOURCES_DIRNAME;
    use super::bundled_executable_path_for_exe;
    use super::copy_from_source_if_needed;
    use super::destination_is_fresh;
    use super::helper_bin_dir;
    use super::helper_version_suffix;
    use super::materialized_file_name;
    use super::replace_helper_file;
    use crate::resolve_exe_for_launch_with_status;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use std::sync::Barrier;
    use tempfile::TempDir;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

    #[test]
    fn destination_is_fresh_uses_content_not_metadata() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let destination = tmp.path().join("destination.exe");

        fs::write(&destination, b"same-size").expect("write destination");
        fs::write(&source, b"same-size").expect("write source");
        let source_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let set_modified = |path: &Path, time| {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open fixture for timestamps")
                .set_times(fs::FileTimes::new().set_modified(time))
                .expect("set fixture timestamp");
        };
        set_modified(&source, source_time);
        set_modified(
            &destination,
            source_time - std::time::Duration::from_secs(2),
        );
        assert!(destination_is_fresh(&source, &destination).expect("identical image despite older metadata"));
        fs::write(&destination, b"evil-size").unwrap();
        assert!(!destination_is_fresh(&source, &destination).unwrap());
        fs::write(&destination, b"same-size").unwrap();

        set_modified(
            &destination,
            source_time + std::time::Duration::from_secs(2),
        );
        assert!(destination_is_fresh(&source, &destination).expect("fresh metadata"));

        // A newer destination of a different size is still stale.
        fs::write(&destination, b"different-size").expect("rewrite destination");
        set_modified(
            &destination,
            source_time + std::time::Duration::from_secs(2),
        );
        assert!(!destination_is_fresh(&source, &destination).expect("size mismatch"));
    }

    #[test]
    fn tampered_materialized_helper_is_revalidated() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.exe");
        let home = tmp.path().join("home");
        fs::write(&source, b"trusted-image").unwrap();
        let (path, _) = resolve_exe_for_launch_with_status(&source, &home);
        fs::write(&path, b"hostile-image").unwrap();
        let (repaired, status) = resolve_exe_for_launch_with_status(&source, &home);
        assert_eq!(repaired, path);
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        assert_eq!(fs::read(repaired).unwrap(), b"trusted-image");
    }

    #[test]
    fn public_resolution_reports_reused_materialization() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let codex_home = tmp.path().join("codex-home");
        fs::write(&source, b"runner-v1").expect("write source");

        let (first_path, first_status) = resolve_exe_for_launch_with_status(&source, &codex_home);
        assert_eq!(first_path.parent(), Some(codex_home.join(".sandbox-bin").as_path()));
        assert!(first_path.file_name().unwrap().to_string_lossy().starts_with("source-"));
        assert_eq!(
            fs::read(&first_path).expect("read first helper"),
            b"runner-v1"
        );
        assert_eq!(HelperMaterializationStatus::ReCopied, first_status);

        let (second_path, second_status) = resolve_exe_for_launch_with_status(&source, &codex_home);
        assert_eq!(first_path, second_path);
        assert_eq!(
            fs::read(&second_path).expect("read reused helper"),
            b"runner-v1"
        );
        assert_eq!(HelperMaterializationStatus::Reused, second_status);
    }

    #[test]
    fn failed_helper_replacement_preserves_existing_destination() {
        let tmp = TempDir::new().expect("tempdir");
        let destination = tmp.path().join("helper.exe");
        let staged = tmp.path().join("staged.exe");
        fs::write(&destination, b"previous helper").expect("write destination");
        fs::write(&staged, b"replacement helper").expect("write staged helper");
        // A reader that does not share DELETE prevents renaming the staged file.
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&staged)
            .expect("lock staged helper against rename");

        let result = replace_helper_file(&staged, &destination);

        assert!(
            result.is_err(),
            "replacement must fail while staged file is locked"
        );
        assert_eq!(
            fs::read(&destination).expect("read old helper"),
            b"previous helper"
        );
        assert_eq!(
            fs::read(&staged).expect("read staged helper"),
            b"replacement helper"
        );
        drop(reader);
    }

    #[test]
    fn public_resolution_replaces_stale_helper_and_cleans_temporary_file() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let codex_home = tmp.path().join("codex-home");
        fs::write(&source, b"old").expect("write source");
        let (destination, _) = resolve_exe_for_launch_with_status(&source, &codex_home);
        fs::write(&source, b"replacement helper").expect("update source");

        let (resolved, status) = resolve_exe_for_launch_with_status(&source, &codex_home);

        assert_ne!(resolved, destination);
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        assert_eq!(
            fs::read(&resolved).expect("read replacement"),
            b"replacement helper"
        );
        let entries = fs::read_dir(helper_bin_dir(&codex_home))
            .expect("read helper directory")
            .map(|entry| entry.expect("helper directory entry").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.into_iter().collect::<std::collections::HashSet<_>>(), [destination, resolved].into_iter().collect());
    }

    #[test]
    fn public_resolution_preserves_locked_helper_and_cleans_temporary_file() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let codex_home = tmp.path().join("codex-home");
        fs::write(&source, b"old").expect("write source");
        let (destination, _) = resolve_exe_for_launch_with_status(&source, &codex_home);
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&destination)
            .expect("lock existing helper against replacement");
        fs::write(&source, b"replacement helper").expect("update source");

        let (resolved, status) = resolve_exe_for_launch_with_status(&source, &codex_home);

        assert_ne!(resolved, destination);
        assert_ne!(resolved, source);
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        assert_eq!(fs::read(&destination).expect("read old helper"), b"old");
        assert_eq!(
            fs::read(&resolved).expect("read fallback helper"),
            b"replacement helper"
        );
        let entries = fs::read_dir(helper_bin_dir(&codex_home))
            .expect("read helper directory")
            .map(|entry| entry.expect("helper directory entry").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.into_iter().collect::<std::collections::HashSet<_>>(), [destination.clone(), resolved.clone()].into_iter().collect());
        let updated_destination = resolved;
        drop(reader);

        let (resolved, status) = resolve_exe_for_launch_with_status(&source, &codex_home);
        assert_eq!(resolved, updated_destination);
        assert_eq!(status, HelperMaterializationStatus::Reused);
        assert_eq!(
            fs::read(&resolved).expect("read recovered helper"),
            b"replacement helper"
        );
    }

    #[test]
    fn public_resolution_replaces_helper_while_shared_reader_keeps_old_contents() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let codex_home = tmp.path().join("codex-home");
        fs::write(&source, b"old").expect("write source");
        let (destination, status) = resolve_exe_for_launch_with_status(&source, &codex_home);
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        let mut reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&destination)
            .expect("open shared helper reader");
        fs::write(&source, b"replacement helper").expect("update source");

        let (resolved, status) = resolve_exe_for_launch_with_status(&source, &codex_home);

        assert_ne!(resolved, destination);
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        assert_eq!(
            fs::read(&resolved).expect("read replacement helper"),
            b"replacement helper"
        );
        let mut previous_contents = Vec::new();
        reader
            .read_to_end(&mut previous_contents)
            .expect("read previous helper through open handle");
        assert_eq!(previous_contents, b"old");
        let entries = fs::read_dir(helper_bin_dir(&codex_home))
            .expect("read helper directory")
            .map(|entry| entry.expect("helper directory entry").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.into_iter().collect::<std::collections::HashSet<_>>(), [destination, resolved].into_iter().collect());
    }

    #[test]
    fn public_resolution_concurrent_replacements_return_complete_helpers() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        let codex_home = tmp.path().join("codex-home");
        fs::write(&source, b"old").expect("write source");
        let (destination, status) = resolve_exe_for_launch_with_status(&source, &codex_home);
        assert_eq!(status, HelperMaterializationStatus::ReCopied);
        let replacement = vec![0x5a; 1024 * 1024];
        fs::write(&source, &replacement).expect("update source");
        let previous_destination = destination;
        let destination = helper_bin_dir(&codex_home).join(format!("source-{}.exe", super::content_digest(&source).unwrap()));
        let start = Barrier::new(8);

        std::thread::scope(|scope| {
            let workers = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        let (resolved, status) =
                            resolve_exe_for_launch_with_status(&source, &codex_home);
                        assert_eq!(resolved, destination);
                        assert!(matches!(
                            status,
                            HelperMaterializationStatus::ReCopied
                                | HelperMaterializationStatus::Reused
                        ));
                        assert_eq!(
                            fs::read(&resolved).expect("read concurrently resolved helper"),
                            replacement
                        );
                    })
                })
                .collect::<Vec<_>>();
            for worker in workers {
                worker.join().expect("helper resolution worker");
            }
        });

        let entries = fs::read_dir(helper_bin_dir(&codex_home))
            .expect("read helper directory")
            .map(|entry| entry.expect("helper directory entry").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.into_iter().collect::<std::collections::HashSet<_>>(), [previous_destination, destination].into_iter().collect());
    }

    #[test]
    fn copy_runner_into_shared_bin_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let codex_home = tmp.path().join("codex-home");
        let source_dir = tmp.path().join("sibling-source");
        fs::create_dir_all(&source_dir).expect("create source dir");
        let runner_source = source_dir.join("codex-command-runner.exe");
        fs::write(&runner_source, b"runner").expect("runner");
        let runner_suffix = helper_version_suffix(&runner_source).expect("runner suffix");
        let runner_destination = helper_bin_dir(&codex_home).join(materialized_file_name(
            HelperExecutable::CommandRunner,
            &runner_suffix,
        ));

        let runner_outcome =
            copy_from_source_if_needed(&runner_source, &runner_destination).expect("runner copy");

        assert_eq!(CopyOutcome::ReCopied, runner_outcome);
        assert_eq!(
            b"runner".as_slice(),
            fs::read(&runner_destination).expect("read runner")
        );
    }

    #[test]
    fn helper_source_lookup_checks_resource_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let release_dir = tmp.path().join("release");
        let resources_dir = release_dir.join(RESOURCES_DIRNAME);
        fs::create_dir_all(&resources_dir).expect("create resources dir");
        let exe = release_dir.join("codex.exe");
        let helper = resources_dir.join("codex-command-runner.exe");
        fs::write(&exe, b"codex").expect("write exe");
        fs::write(&helper, b"runner").expect("write helper");

        let resolved =
            bundled_executable_path_for_exe(&exe, /*file_name*/ "codex-command-runner.exe")
                .expect("helper path");

        assert_eq!(resolved, helper);
    }

    #[test]
    fn helper_source_lookup_checks_package_resource_dir_for_bin_exe() {
        let tmp = TempDir::new().expect("tempdir");
        let package_dir = tmp.path().join("package");
        let bin_dir = package_dir.join(BIN_DIRNAME);
        let resources_dir = package_dir.join(RESOURCES_DIRNAME);
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        fs::create_dir_all(&resources_dir).expect("create resources dir");
        let exe = bin_dir.join("codex.exe");
        let helper = resources_dir.join("codex-command-runner.exe");
        fs::write(&exe, b"codex").expect("write exe");
        fs::write(&helper, b"runner").expect("write helper");

        let resolved =
            bundled_executable_path_for_exe(&exe, /*file_name*/ "codex-command-runner.exe")
                .expect("helper path");

        assert_eq!(resolved, helper);
    }

    #[test]
    fn helper_source_lookup_prefers_package_resource_dir_over_bin_resource_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let package_dir = tmp.path().join("package");
        let bin_dir = package_dir.join(BIN_DIRNAME);
        let package_resources_dir = package_dir.join(RESOURCES_DIRNAME);
        let bin_resources_dir = bin_dir.join(RESOURCES_DIRNAME);
        fs::create_dir_all(&package_resources_dir).expect("create package resources dir");
        fs::create_dir_all(&bin_resources_dir).expect("create bin resources dir");
        let exe = bin_dir.join("codex.exe");
        let package_helper = package_resources_dir.join("codex-command-runner.exe");
        let bin_helper = bin_resources_dir.join("codex-command-runner.exe");
        fs::write(&exe, b"codex").expect("write exe");
        fs::write(&package_helper, b"package runner").expect("write package helper");
        fs::write(&bin_helper, b"bin runner").expect("write bin helper");

        let resolved =
            bundled_executable_path_for_exe(&exe, /*file_name*/ "codex-command-runner.exe")
                .expect("helper path");

        assert_eq!(resolved, package_helper);
    }

    #[test]
    fn helper_source_lookup_prefers_direct_sibling_over_resource_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let release_dir = tmp.path().join("release");
        let resources_dir = release_dir.join(RESOURCES_DIRNAME);
        fs::create_dir_all(&resources_dir).expect("create resources dir");
        let exe = release_dir.join("codex.exe");
        let sibling_helper = release_dir.join("codex-command-runner.exe");
        let resource_helper = resources_dir.join("codex-command-runner.exe");
        fs::write(&exe, b"codex").expect("write exe");
        fs::write(&sibling_helper, b"sibling runner").expect("write sibling helper");
        fs::write(&resource_helper, b"resource runner").expect("write resource helper");

        let resolved =
            bundled_executable_path_for_exe(&exe, /*file_name*/ "codex-command-runner.exe")
                .expect("helper path");

        assert_eq!(resolved, sibling_helper);
    }

    #[test]
    fn helper_version_suffix_uses_cli_version_or_dev_build_metadata() {
        let tmp = TempDir::new().expect("tempdir");
        let source = tmp.path().join("source.exe");
        fs::write(&source, b"runner-v1").expect("write source");
        // Pin the mtime so the dev-build suffix is known: 9 bytes, 1_700_000_000 s = 0x6553f100.
        fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .expect("open source for timestamps")
            .set_times(fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ))
            .expect("set source timestamp");
        let suffix = helper_version_suffix(&source).expect("suffix");

        let digest = super::content_digest(&source).unwrap();
        assert_eq!(suffix, format!("{}-{digest}", env!("CARGO_PKG_VERSION")));
        fs::write(&source, b"runner-v2").unwrap();
        assert_ne!(helper_version_suffix(&source).unwrap(), suffix);
    }

    #[test]
    fn materialized_file_name_adds_suffix_before_extension() {
        let file_name = materialized_file_name(HelperExecutable::CommandRunner, "test-suffix");

        assert_eq!(file_name, "codex-command-runner-test-suffix.exe");
    }
}
