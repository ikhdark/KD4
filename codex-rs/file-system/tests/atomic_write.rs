use std::path::Path;
use std::path::PathBuf;

// Compile the private staging fault-injection test in this existing integration
// target; the library intentionally disables its standalone unit-test target.
#[allow(dead_code)]
#[path = "../src/atomic_write.rs"]
mod atomic_write;

#[test]
fn resolution_preserves_metadata_errors() {
    let error = codex_file_system::resolve_symlink_write_paths(Path::new("invalid\0path"))
        .err()
        .expect("invalid paths must not become a replacement plan");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn resolution_rejects_cycles_and_preserves_dangling_targets() {
    use std::os::windows::fs::symlink_file;
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    symlink_file("second", &first).unwrap();
    let paths = codex_file_system::resolve_symlink_write_paths(&first).unwrap();
    assert_eq!(paths.read_path.as_ref(), Some(&second));
    assert_eq!(paths.write_path, second);
    codex_file_system::write_atomically(&paths.write_path, "target contents").unwrap();
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "target contents");
    assert_eq!(std::fs::read_link(&first).unwrap(), PathBuf::from("second"));
    std::fs::remove_file(&second).unwrap();
    symlink_file("first", &second).unwrap();
    let error = codex_file_system::resolve_symlink_write_paths(&first)
        .err()
        .expect("cycle must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("symlink cycle"));
    assert_eq!(std::fs::read_link(&first).unwrap(), PathBuf::from("second"));
    assert_eq!(std::fs::read_link(&second).unwrap(), PathBuf::from("first"));
}

#[test]
fn lock_path_is_stable_for_all_writers() {
    assert_eq!(
        codex_file_system::atomic_write_lock_path(Path::new("config.toml")).unwrap(),
        PathBuf::from(".config.toml.lock")
    );
}

#[test]
fn patch_capture_admission_needs_only_read_access_to_initialized_lock() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.txt");
    let lock_path = codex_file_system::prepare_patch_capture(&path).unwrap();
    // Forbid write opens independently of the caller's token. Both shared and
    // exclusive byte-range locks work on a handle opened with read access.
    let read_only = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&lock_path)
        .unwrap();
    let first = codex_file_system::guard_patch_read(&path).unwrap().unwrap();
    let second = codex_file_system::guard_patch_read(&path).unwrap().unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        codex_file_system::try_acquire_patch_capture(&path)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock,
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    drop((first, second));
    let writer = codex_file_system::try_acquire_patch_capture(&path).unwrap();
    assert_eq!(
        codex_file_system::guard_patch_read(&path)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock,
    );
    drop(writer);
    assert!(
        codex_file_system::guard_patch_read(&path)
            .unwrap()
            .is_some()
    );
    drop(read_only);
    assert!(
        !path.exists(),
        "read admission must not create source files"
    );
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn resolved_config_aliases_hold_the_same_target_lock() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("settings.toml");
    let alias = temp.path().join("config.toml");
    std::fs::write(&target, "initial").unwrap();
    std::os::windows::fs::symlink_file(&target, &alias).unwrap();
    let (paths, lock) = codex_file_system::acquire_resolved_atomic_write_lock(&alias).unwrap();
    assert_eq!(
        paths.write_path,
        std::fs::canonicalize(temp.path())
            .unwrap()
            .join("settings.toml")
    );
    assert!(
        codex_file_system::try_acquire_atomic_write_lock(&target)
            .unwrap()
            .is_none()
    );
    drop(lock);
    assert!(
        codex_file_system::try_acquire_atomic_write_lock(&target)
            .unwrap()
            .is_some()
    );
    assert_eq!(std::fs::read_link(&alias).unwrap(), target);
}

#[test]
fn raw_symlink_targets_do_not_expand_home() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("~")).unwrap();
    std::fs::create_dir(temp.path().join("~name")).unwrap();
    for payload in [r"~\note.txt", "~/note.txt", "~name/note.txt"] {
        let link = temp.path().join("alias");
        let target = temp.path().join(payload);
        std::os::windows::fs::symlink_file(payload, &link).unwrap();
        let paths = codex_file_system::resolve_symlink_write_paths(&link).unwrap();
        // Check the destination before writing: an unfixed test must never
        // publish into the real process home.
        assert_eq!(paths.write_path, target);
        codex_file_system::write_bytes_atomically_without_sync(&paths.write_path, b"\0\xffnew")
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"\0\xffnew");
        if payload.contains('/') {
            // Windows stores this raw payload but its native link traversal
            // does not normalize forward slashes. The write resolver does.
            assert_eq!(std::fs::read(&link).unwrap_err().raw_os_error(), Some(123));
        } else {
            assert_eq!(std::fs::read(&link).unwrap(), b"\0\xffnew");
        }
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from(payload));
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&target).unwrap();
    }
}

#[test]
fn atomically_replaces_existing_contents() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("state.json");
    let alias = temp.path().join("original");
    for (contents, sync) in [
        (b"new".as_slice(), true),
        (&[0, 1, 2, 255], true),
        (b"new".as_slice(), false),
    ] {
        std::fs::write(&target, b"old").unwrap();
        std::fs::hard_link(&target, &alias).unwrap();
        let _lock = codex_file_system::acquire_atomic_write_lock(&target).unwrap();
        if !sync {
            codex_file_system::write_bytes_atomically_without_sync(&target, contents).unwrap();
        } else if contents == b"new" {
            codex_file_system::write_atomically(&target, "new").unwrap();
        } else {
            codex_file_system::write_bytes_atomically(&target, contents).unwrap();
        }
        assert_eq!(std::fs::read(&target).unwrap(), contents);
        // A truncating write has the same final bytes but incorrectly mutates
        // every link to the old object instead of replacing this entry.
        assert_eq!(std::fs::read(&alias).unwrap(), b"old");
        std::fs::remove_file(&alias).unwrap();
    }
}

#[test]
fn failed_replacement_preserves_destination_and_removes_staging_file() {
    let temp = tempfile::tempdir().unwrap();
    // A file cannot replace a non-empty directory, so publication fails after staging.
    let target = temp.path().join("state.bin");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("kept"), b"kept").unwrap();

    codex_file_system::write_bytes_atomically(&target, b"new")
        .expect_err("a file must not replace a directory");

    assert_eq!(std::fs::read(target.join("kept")).unwrap(), b"kept");
    let names = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, vec![std::ffi::OsString::from("state.bin")]);
}

#[test]
fn missing_target_can_be_created_after_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("new.toml");
    let paths = codex_file_system::resolve_symlink_write_paths(&target).unwrap();
    assert_eq!(paths.read_path.as_deref(), Some(target.as_path()));
    assert_eq!(paths.write_path, target);
    codex_file_system::write_atomically(&paths.write_path, "enabled = true").unwrap();
    assert_eq!(std::fs::read_to_string(target).unwrap(), "enabled = true");
}

#[test]
fn unsynced_write_preserves_read_only_and_missing_parent() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("readonly.txt");
    std::fs::write(&target, "before").unwrap();
    let original_permissions = std::fs::metadata(&target).unwrap().permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&target, readonly).unwrap();
    let result = codex_file_system::write_bytes_atomically_without_sync(&target, b"after");
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"before");
    assert!(std::fs::metadata(&target).unwrap().permissions().readonly());
    std::fs::set_permissions(&target, original_permissions).unwrap();
    let missing = temp.path().join("missing/child.txt");
    assert_eq!(
        codex_file_system::write_bytes_atomically_without_sync(&missing, b"after")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(!missing.parent().unwrap().exists());
}

#[test]
fn unsynced_failed_write_does_not_truncate_or_leak_staging() {
    use std::os::windows::fs::OpenOptionsExt;
    // Both handles deny delete sharing. Mode 3 still allows writing, exposing
    // an incorrect fallback to truncation that mode 1 would also block.
    for share_mode in [1, 3] {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("locked.txt");
        std::fs::write(&target, "before").unwrap();
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(share_mode)
            .open(&target)
            .unwrap();
        let error = codex_file_system::write_bytes_atomically_without_sync(&target, b"after")
            .expect_err("publication must not fall back to truncation");
        // Replacement can report access denied or sharing violation on Windows.
        assert!(
            matches!(error.raw_os_error(), Some(5 | 32)),
            "share mode {share_mode}: {error}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"before");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}

#[test]
fn conditional_replacement_keeps_security_that_a_new_file_cannot_receive() {
    use codex_file_system::FileMutationOutcome;
    use codex_file_system::mutate_file_if_matches;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::SetFileSecurityW;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }
    // Owner, group and DACL exactly as stored for the object at `path`.
    fn security(path: &Path) -> Vec<u32> {
        let path = wide(path);
        let information =
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut needed = 0;
        // SAFETY: the path is NUL-terminated; a null buffer only queries the size.
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                information,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        assert!(needed > 0, "{}", std::io::Error::last_os_error());
        let mut descriptor = vec![0_u32; (needed as usize).div_ceil(size_of::<u32>())];
        let buffer = descriptor.as_mut_ptr().cast();
        // SAFETY: the DWORD-aligned buffer holds at least `needed` bytes.
        let read =
            unsafe { GetFileSecurityW(path.as_ptr(), information, buffer, needed, &mut needed) };
        assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
        descriptor
    }

    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("no-security-writes");
    std::fs::create_dir(&directory).unwrap();
    // Like a sandbox writable root: Authenticated Users may modify, rename and
    // delete children, but no ACE grants WRITE_DAC or WRITE_OWNER, and OWNER
    // RIGHTS removes the creator's implicit WRITE_DAC. The replaced file's
    // owner and DACL therefore cannot be installed on a staged file.
    let sddl: Vec<u16> = "D:P(A;OICI;0x1301bf;;;S-1-5-11)(A;OICI;0x1301bf;;;S-1-3-4)"
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the SDDL is NUL-terminated and the output slot is writable.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(converted, 0, "{}", std::io::Error::last_os_error());
    let directory_path = wide(&directory);
    let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    // SAFETY: the path is NUL-terminated and the converted descriptor is live.
    let applied = unsafe { SetFileSecurityW(directory_path.as_ptr(), information, descriptor) };
    let apply_error = std::io::Error::last_os_error();
    // SAFETY: the conversion allocated the descriptor, which is no longer used.
    unsafe { LocalFree(descriptor) };
    assert_ne!(applied, 0, "{apply_error}");

    let target = directory.join("file.txt");
    std::fs::write(&target, b"old").unwrap();
    let original = security(&target);
    // Growing and shrinking must both leave exactly the replacement bytes.
    let steps: [(&[u8], &[u8]); 2] = [
        (b"old", b"a longer replacement"),
        (b"a longer replacement", b"short"),
    ];
    for (current, replacement) in steps {
        let outcome = mutate_file_if_matches(&target, Some(current), Some(replacement)).unwrap();
        assert!(
            matches!(outcome, FileMutationOutcome::Committed),
            "{outcome:?}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), replacement);
        // The replaced file's owner, group and DACL, not a new file's.
        assert_eq!(security(&target), original);
    }
    // No staging, recovery or preimage file remains.
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

    // Rewriting a linked object would also change its other name: fail closed.
    let alias = directory.join("alias.txt");
    std::fs::hard_link(&target, &alias).unwrap();
    let error = mutate_file_if_matches(&target, Some(b"short".as_slice()), Some(b"new".as_slice()))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&target).unwrap(), b"short");
    assert_eq!(std::fs::read(&alias).unwrap(), b"short");
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
}
