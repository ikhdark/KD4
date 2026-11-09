use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;

pub struct SymlinkWritePaths {
    pub read_path: Option<PathBuf>,
    pub write_path: PathBuf,
}

pub fn resolve_symlink_write_paths(path: &Path) -> io::Result<SymlinkWritePaths> {
    let mut current = AbsolutePathBuf::from_absolute_path(path)
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    let mut visited = HashSet::new();

    loop {
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SymlinkWritePaths {
                    read_path: Some(current.clone()),
                    write_path: current,
                });
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(SymlinkWritePaths {
                read_path: Some(current.clone()),
                write_path: current,
            });
        }
        if !visited.insert(current.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("symlink cycle while resolving {}", path.display()),
            ));
        }
        let target = std::fs::read_link(&current)?;
        let next = if target.is_absolute() {
            AbsolutePathBuf::from_absolute_path(&target)
        } else if let Some(parent) = current.parent() {
            Ok(AbsolutePathBuf::resolve_literal_path_against_base(
                &target, parent,
            ))
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("symlink {} has no parent directory", current.display()),
            ));
        };
        current = next?.into_path_buf();
    }
}

pub struct AtomicWriteLock {
    file: File,
}

impl Drop for AtomicWriteLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub fn atomic_write_lock_path(write_path: &Path) -> io::Result<PathBuf> {
    let file_name = write_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no file name", write_path.display()),
        )
    })?;
    let mut lock_name = OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".lock");
    Ok(write_path.with_file_name(lock_name))
}

pub fn acquire_atomic_write_lock(write_path: &Path) -> io::Result<AtomicWriteLock> {
    let file = open_atomic_write_lock(write_path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(AtomicWriteLock { file })
}

/// Lock the resolved destination, then recheck aliases before reading or writing.
/// All cooperative config writers must use the same physical target lock.
pub fn acquire_resolved_atomic_write_lock(
    path: &Path,
) -> io::Result<(SymlinkWritePaths, AtomicWriteLock)> {
    let resolve = || -> io::Result<SymlinkWritePaths> {
        let mut paths = resolve_symlink_write_paths(path)?;
        let parent = paths.write_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "write target has no parent")
        })?;
        std::fs::create_dir_all(parent)?;
        let name = paths.write_path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "write target has no file name")
        })?;
        paths.write_path = std::fs::canonicalize(parent)?.join(name);
        paths.read_path = Some(paths.write_path.clone());
        Ok(paths)
    };
    let paths = resolve()?;
    let lock = acquire_atomic_write_lock(&paths.write_path)?;
    if resolve()?.write_path != paths.write_path {
        return Err(io::Error::other(
            "write target changed while acquiring its persistence lock; retry the edit",
        ));
    }
    Ok((paths, lock))
}

/// Attempts the same transaction lock without waiting for another writer.
/// `None` means contention; filesystem errors are still reported to the caller.
pub fn try_acquire_atomic_write_lock(write_path: &Path) -> io::Result<Option<AtomicWriteLock>> {
    let file = open_atomic_write_lock(write_path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(AtomicWriteLock { file })),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn open_atomic_write_lock(write_path: &Path) -> io::Result<File> {
    let lock_path = atomic_write_lock_path(write_path)?;
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
}

pub fn write_atomically(write_path: &Path, contents: &str) -> io::Result<()> {
    write_bytes_atomically(write_path, contents.as_bytes())
}

/// Replaces the destination after synchronizing a same-directory temporary file.
/// Parent-directory synchronization is best-effort on Windows. An error after
/// replacement does not imply that the old contents remain installed.
pub fn write_bytes_atomically(write_path: &Path, contents: &[u8]) -> io::Result<()> {
    write_bytes_atomically_impl(write_path, contents, true)
}

/// Durable replacement retaining the destination's permissions and Windows DACL.
/// Resolve symlinks before calling. Pre-publication failures retain the old file.
pub fn write_bytes_atomically_preserving_permissions(
    write_path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    write_bytes_atomically(write_path, contents)
}

/// Same-directory replacement without durability barriers or parent creation.
/// Preserves existing permissions. Sharing/access failures leave the original
/// intact; never fall back to truncating an existing file.
/// The caller resolves symlinks first. Replacement changes only the addressed
/// directory entry: other hardlinks deliberately retain the original bytes.
pub fn write_bytes_atomically_without_sync(write_path: &Path, contents: &[u8]) -> io::Result<()> {
    write_bytes_atomically_impl(write_path, contents, false)
}

fn write_bytes_atomically_impl(write_path: &Path, contents: &[u8], sync: bool) -> io::Result<()> {
    stage_and_replace(write_path, sync, |temporary| temporary.write_all(contents))
}

fn stage_and_replace(
    write_path: &Path,
    sync: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = write_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no parent directory", write_path.display()),
        )
    })?;
    if sync {
        std::fs::create_dir_all(parent)?;
    }
    let permissions = match std::fs::metadata(write_path) {
        Ok(metadata) => {
            if metadata.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination is read-only",
                ));
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut temporary = NamedTempFile::new_in(parent)?;
    if permissions.is_some() {
        copy_file_security(write_path, temporary.path())?;
    }
    write(temporary.as_file_mut())?;
    temporary.flush()?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    if sync {
        temporary.as_file().sync_all()?;
    }
    // Clear Windows temporary attributes, then retain cleanup on rename failure.
    let (_file, path) = temporary.keep().map_err(|error| error.error)?;
    let path = tempfile::TempPath::try_from_path(path)?;
    // std's Windows rename supports replacing a file held by an open reader.
    std::fs::rename(&path, write_path)?;
    if !sync {
        return Ok(());
    }
    sync_parent_directory(parent).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} was replaced, but synchronizing its parent directory failed: {error}",
                write_path.display()
            ),
        )
    })?;
    Ok(())
}

// std::fs::Permissions carries only the read-only flag on Windows. Preserve
// ownership and the DACL too, before any replacement can make the new file live.
// Inability to read or install the descriptor is a failure, not permission to
// replace a protected file with a differently secured temporary file.
pub(crate) fn copy_file_security(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;
    use windows_sys::Win32::Security::GetSecurityDescriptorControl;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::SE_DACL_PROTECTED;
    use windows_sys::Win32::Security::SetFileSecurityW;
    use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0;
    // SAFETY: both paths are NUL-terminated and the size output is writable.
    let result = unsafe {
        GetFileSecurityW(
            source.as_ptr(),
            information,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    if needed == 0 {
        return Err(io::Error::other("empty destination security descriptor"));
    }
    // A self-relative security descriptor requires DWORD alignment.
    let mut descriptor = vec![0_u32; (needed as usize).div_ceil(size_of::<u32>())];
    // SAFETY: the buffer is aligned, live, and at least `needed` bytes long.
    if unsafe {
        GetFileSecurityW(
            source.as_ptr(),
            information,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: GetFileSecurityW initialized the descriptor and outputs are valid.
    if unsafe {
        GetSecurityDescriptorControl(descriptor.as_mut_ptr().cast(), &mut control, &mut revision)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: the destination path and initialized descriptor remain live.
    if unsafe {
        SetFileSecurityW(
            destination.as_ptr(),
            information | protection,
            descriptor.as_mut_ptr().cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Close the exclusive handle before unlinking its path, including error exits.
/// NamedTempFile's path-first drop order cannot unlink a share-denied handle.
pub(crate) struct SourceDaclPreimage {
    file: File,
    path: tempfile::TempPath,
}

impl SourceDaclPreimage {
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    pub(crate) fn into_temp_path(self) -> tempfile::TempPath {
        drop(self.file);
        self.path
    }
}

/// A fallback copy must not expose plaintext under inherited parent defaults.
/// Install the original DACL at creation (before bytes), without assigning its
/// owner or requiring WRITE_DAC on a sandbox-owned staged file. Keep the DACL
/// protected so broader inheritable ACEs cannot be added to the recovery copy.
pub(crate) fn create_preimage_with_source_dacl(
    source: &File,
    parent: &Path,
) -> io::Result<SourceDaclPreimage> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, InitializeSecurityDescriptor, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_TEMPORARY,
    };
    struct Descriptor(*mut std::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocated this descriptor using LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let mut descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: source owns the exclusive READ_CONTROL-capable handle. Reading
    // security through this handle does not reopen its denied-share pathname.
    let error = unsafe {
        GetSecurityInfo(
            source.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "GetSecurityInfo of exclusive original: {}",
                io::Error::from_raw_os_error(error as i32)
            ),
        ));
    }
    let descriptor = Descriptor(descriptor);
    // GetSecurityInfo's allocation can include owner/group metadata even when
    // only its DACL was requested. A new copy must use the executor's default
    // owner, not attempt assignment of an administrative source owner.
    let mut copy = SECURITY_DESCRIPTOR::default();
    // SAFETY: initialized native descriptor; the referenced DACL allocation
    // remains owned by `descriptor` throughout CREATE_NEW.
    if unsafe { InitializeSecurityDescriptor((&mut copy as *mut SECURITY_DESCRIPTOR).cast(), 1) }
        == 0
        || unsafe {
            SetSecurityDescriptorDacl((&mut copy as *mut SECURITY_DESCRIPTOR).cast(), 1, dacl, 0)
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: initialized, owned security descriptor remains live until after creation.
    if unsafe {
        SetSecurityDescriptorControl(
            (&mut copy as *mut SECURITY_DESCRIPTOR).cast(),
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("protect source DACL descriptor: {error}"),
        ));
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut copy as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let result = tempfile::Builder::new()
        .prefix(".codex-patch-preimage-")
        .make_in(parent, |path| {
            let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            // SAFETY: valid live security descriptor/path; CREATE_NEW never replaces
            // an existing object. Exclusive sharing protects bytes until settlement.
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    &security,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_TEMPORARY,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let error = io::Error::last_os_error();
                return Err(io::Error::new(
                    error.kind(),
                    format!("CREATE_NEW source-DACL recovery copy: {error}"),
                ));
            }
            // SAFETY: this uniquely owned native handle is transferred exactly once.
            Ok(unsafe { File::from_raw_handle(handle.cast()) })
        });
    drop(descriptor);
    result.map(|temporary| {
        let (file, path) = temporary.into_parts();
        SourceDaclPreimage { file, path }
    })
}

fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .and_then(|directory| directory.sync_all());
    match result {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}
