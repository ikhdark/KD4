use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::SystemTime;
use tempfile::NamedTempFile;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

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
    // Every sandboxed filesystem call launches this executable. Hashing the
    // ~300 MB source and copy each time cost seconds per call, so reuse a copy
    // verified earlier in this process while the source file is unchanged.
    let bin_dir = helper_bin_dir(codex_home);
    let slot = verified_copy_slot(source, &bin_dir);
    let mut verified = slot.lock().unwrap_or_else(PoisonError::into_inner);
    let source_stamp = file_stamp(source).ok();
    if let Some(copy) = verified.as_ref()
        && source_stamp == Some(copy.source_stamp)
    {
        return (copy.destination.clone(), HelperMaterializationStatus::Reused);
    }
    *verified = None;
    let digest = match content_digest(source) {
        Ok(digest) => digest,
        Err(_) => return (source.to_path_buf(), HelperMaterializationStatus::SourceFallback),
    };
    let mut name = source.file_stem().unwrap_or(file_name).to_os_string();
    name.push(format!("-{digest}"));
    if let Some(extension) = source.extension() { name.push("."); name.push(extension); }
    let destination = bin_dir.join(name);
    let pinned = pin_matching_copy(&destination, &digest).ok().flatten();
    let outcome = if pinned.is_some() {
        Ok(CopyOutcome::Reused)
    } else {
        copy_from_source_with_digest(source, &destination, &digest)
    };
    match outcome {
        Ok(outcome) => {
            let pin = pinned.or_else(|| pin_matching_copy(&destination, &digest).ok().flatten());
            if let (Some(stamp), Some(pin)) = (source_stamp, pin)
                && file_stamp(source).is_ok_and(|after| after == stamp)
            {
                *verified = Some(VerifiedCopy {
                    source_stamp: stamp,
                    destination: destination.clone(),
                    _pin: pin,
                });
            }
            let status = match outcome {
                CopyOutcome::Reused => HelperMaterializationStatus::Reused,
                CopyOutcome::ReCopied => HelperMaterializationStatus::ReCopied,
            };
            (destination, status)
        }
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


fn destination_matches_digest(destination: &Path, expected: &str) -> Result<bool> {
    match content_digest(destination) {
        Ok(digest) => Ok(digest == expected),
        Err(err) if err.downcast_ref::<std::io::Error>().is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => Err(err),
    }
}

fn content_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open helper image {}", path.display()))?;
    Ok(reader_digest(&mut file)?)
}

fn reader_digest(reader: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 { break; }
        hash.update(&bytes[..count]);
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// An executable copy verified against its source digest in this process.
struct VerifiedCopy {
    source_stamp: FileStamp,
    destination: PathBuf,
    /// Shares only reads, so the verified bytes cannot be rewritten, renamed or
    /// deleted while this process reuses the copy without hashing it again.
    _pin: fs::File,
}

type VerifiedCopySlot = Arc<Mutex<Option<VerifiedCopy>>>;

/// One slot per source and helper directory; concurrent first launches wait
/// for a single verification instead of hashing the same image in parallel.
fn verified_copy_slot(source: &Path, bin_dir: &Path) -> VerifiedCopySlot {
    static SLOTS: OnceLock<Mutex<HashMap<(PathBuf, PathBuf), VerifiedCopySlot>>> = OnceLock::new();
    let mut slots = SLOTS.get_or_init(Default::default).lock().unwrap_or_else(PoisonError::into_inner);
    slots.entry((source.to_path_buf(), bin_dir.to_path_buf())).or_default().clone()
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: SystemTime,
    created: SystemTime,
}

fn file_stamp(path: &Path) -> std::io::Result<FileStamp> {
    let metadata = fs::metadata(path)?;
    Ok(FileStamp { len: metadata.len(), modified: metadata.modified()?, created: metadata.created()? })
}

/// Opens `destination` without write or delete sharing and returns the handle
/// only when the bytes read through it match `expected`.
fn pin_matching_copy(destination: &Path, expected: &str) -> std::io::Result<Option<fs::File>> {
    let mut file = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(destination)?;
    Ok((reader_digest(&mut file)? == expected).then_some(file))
}

