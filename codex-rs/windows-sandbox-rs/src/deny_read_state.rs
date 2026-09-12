use crate::acl::revoke_deny_read_ace;
use crate::deny_read_acl::apply_planned_deny_read_acls;
use crate::deny_read_acl::lexical_path_key;
use crate::deny_read_acl::plan_deny_read_acl_paths;
use crate::setup::sandbox_dir;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const DENY_READ_ACL_STATE_FILE: &str = "deny_read_acl_state.json";

#[derive(Default, Deserialize, Serialize)]
struct PersistentDenyReadAclState {
    principals: BTreeMap<String, Vec<PathBuf>>,
}

/// Reconciles the persistent deny-read ACEs owned by one sandbox principal.
///
/// Workspace-write and elevated sandbox sessions intentionally leave ACLs in
/// place after a command exits, because descendants may outlive the launcher.
/// That makes the ACL set stateful across runs. Persist the paths applied for
/// each SID before applying them, apply the new desired set first, and only then
/// revoke stale paths. The durable union remains available for retry if application,
/// revocation, or the final state update fails.
///
/// # Safety
/// Caller must pass a valid SID pointer matching `principal_sid`.
pub unsafe fn sync_persistent_deny_read_acls(
    codex_home: &Path,
    principal_sid: &str,
    desired_paths: &[PathBuf],
    psid: *mut c_void,
) -> Result<Vec<PathBuf>> {
    let state_path = sandbox_dir(codex_home).join(DENY_READ_ACL_STATE_FILE);
    let state_dir = state_path
        .parent()
        .context("deny-read state has no parent")?;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create deny-read state directory {}", state_dir.display()))?;
    // Different principals share one file. Keep its read/intent/ACL/commit sequence
    // serialized across launcher and elevated-helper processes.
    let state_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_path.with_extension("lock"))
        .context("open deny-read ACL state lock")?;
    state_lock.lock().context("lock deny-read ACL state")?;
    let mut state = load_state(&state_path)?;
    let previous_paths = state
        .principals
        .get(principal_sid)
        .cloned()
        .unwrap_or_default();

    // Resolve once: every path that can receive an ACE must already be in the
    // durable intent, including existing canonical targets of lexical denies.
    let planned_paths = plan_deny_read_acl_paths(desired_paths);
    for path in &planned_paths {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("validate deny-read ACL path {}", path.display()));
            }
        }
    }
    let mut pending_paths = previous_paths.clone();
    let mut pending_keys = pending_paths
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<HashSet<_>>();
    for path in &planned_paths {
        if pending_keys.insert(lexical_path_key(path)) {
            pending_paths.push(path.clone());
        }
    }
    state
        .principals
        .insert(principal_sid.to_string(), pending_paths);
    store_state(&state_path, &state)
        .context("persist deny-read ACL intent before applying ACEs")?;

    let applied_paths = unsafe { apply_planned_deny_read_acls(planned_paths, psid) }?;
    let desired_keys = applied_paths
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<HashSet<_>>();

    let mut failed_stale_paths = Vec::new();
    let mut revoke_errors = Vec::new();
    for path in previous_paths {
        if desired_keys.contains(&lexical_path_key(&path)) {
            continue;
        }
        // A persisted intent may name a path whose creation failed. Absence has
        // no ACE left to revoke; other lookup errors must retain its retry entry.
        let revoke_result = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(anyhow::Error::from(error)),
            Ok(_) => unsafe { revoke_deny_read_ace(&path, psid) },
        };
        if let Err(error) = revoke_result {
            revoke_errors.push(format!("{}: {error}", path.display()));
            failed_stale_paths.push(path);
        }
    }

    let mut persisted_paths = applied_paths.clone();
    let mut persisted_keys = desired_keys;
    for path in failed_stale_paths {
        if persisted_keys.insert(lexical_path_key(&path)) {
            persisted_paths.push(path);
        }
    }

    if persisted_paths.is_empty() {
        state.principals.remove(principal_sid);
    } else {
        state
            .principals
            .insert(principal_sid.to_string(), persisted_paths);
    }
    store_state(&state_path, &state)?;

    if !revoke_errors.is_empty() {
        anyhow::bail!(
            "failed to revoke stale deny-read ACLs; retained for retry: {}",
            revoke_errors.join("; ")
        );
    }

    Ok(applied_paths)
}

fn load_state(path: &Path) -> Result<PersistentDenyReadAclState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse deny-read ACL state {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistentDenyReadAclState::default())
        }
        Err(err) => {
            Err(err).with_context(|| format!("read deny-read ACL state {}", path.display()))
        }
    }
}

fn store_state(path: &Path, state: &PersistentDenyReadAclState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serialize deny-read ACL state")?;
    let parent = path.parent().context("deny-read ACL state has no parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("stage deny-read ACL state {}", path.display()))?;
    staged
        .write_all(&bytes)
        .with_context(|| format!("write deny-read ACL state {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("sync deny-read ACL state {}", path.display()))?;
    let staged = staged.into_temp_path();
    std::fs::rename(&staged, path)
        .with_context(|| format!("replace deny-read ACL state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::token::LocalSid;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    fn has_deny(path: &Path, sid: &LocalSid) -> Result<bool> {
        // SAFETY: LocalSid owns a valid SID; fetch returns a live descriptor
        // backing its DACL, which is freed only after inspection.
        unsafe {
            let (dacl, descriptor) = fetch_dacl_handle(path)?;
            let result = dacl_has_read_deny_for_sid(dacl, sid.as_ptr());
            if !descriptor.is_null() {
                LocalFree(descriptor);
            }
            Ok(result)
        }
    }

    #[test]
    fn persistent_denies_write_failure_preserves_acls_and_retry_reconciles() -> Result<()> {
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let old = workspace.path().join("old-secret");
        let new = workspace.path().join("new-secret");
        std::fs::write(&old, b"old bytes")?;
        std::fs::write(&new, b"new bytes")?;
        let principal = crate::cap::load_or_create_cap_sids(home.path())?.readonly;
        let sid = LocalSid::from_string(&principal)?;
        assert!(!has_deny(&old, &sid)?);
        assert!(!has_deny(&new, &sid)?);
        // SAFETY: the owned SID matches the persisted principal string.
        unsafe {
            sync_persistent_deny_read_acls(home.path(), &principal, &[old.clone()], sid.as_ptr())?;
        }
        assert!(has_deny(&old, &sid)?);
        let state_path = sandbox_dir(home.path()).join(DENY_READ_ACL_STATE_FILE);
        let before = std::fs::read(&state_path)?;
        let invalid = workspace.path().join("invalid\0path");
        assert!(
            unsafe {
                sync_persistent_deny_read_acls(home.path(), &principal, &[invalid], sid.as_ptr())
            }
            .is_err()
        );
        assert_eq!(
            std::fs::read(&state_path)?,
            before,
            "invalid paths must not poison durable retry state"
        );
        assert!(has_deny(&old, &sid)?);
        // Native sharing allows the normal read but rejects write/replacement.
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&state_path)?;
        for _ in 0..2 {
            let error = unsafe {
                sync_persistent_deny_read_acls(
                    home.path(),
                    &principal,
                    &[new.clone()],
                    sid.as_ptr(),
                )
            }
            .expect_err("tracking failure must precede all ACL changes");
            assert!(format!("{error:#}").contains("deny-read ACL"));
            assert!(has_deny(&old, &sid)?);
            assert!(
                !has_deny(&new, &sid)?,
                "failed tracking must not leave an untracked deny"
            );
            assert_eq!(std::fs::read(&state_path)?, before);
        }
        drop(locked);
        unsafe {
            sync_persistent_deny_read_acls(home.path(), &principal, &[new.clone()], sid.as_ptr())?;
        }
        assert!(!has_deny(&old, &sid)?);
        assert!(has_deny(&new, &sid)?);
        let stored = load_state(&state_path)?;
        assert_eq!(stored.principals.get(&principal), Some(&vec![new.clone()]));
        unsafe {
            sync_persistent_deny_read_acls(home.path(), &principal, &[], sid.as_ptr())?;
        }
        assert!(!has_deny(&new, &sid)?);
        assert!(!load_state(&state_path)?.principals.contains_key(&principal));
        assert_eq!(std::fs::read(&old)?, b"old bytes");
        assert_eq!(std::fs::read(&new)?, b"new bytes");
        Ok(())
    }

    fn native_entries_for_sid(path: &Path, sid: &LocalSid) -> Result<Vec<(u8, u8, u32)>> {
        use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
        use windows_sys::Win32::Security::ACCESS_DENIED_ACE;
        use windows_sys::Win32::Security::ACE_HEADER;
        use windows_sys::Win32::Security::EqualSid;
        use windows_sys::Win32::Security::GetAce;
        unsafe {
            let (dacl, descriptor) = fetch_dacl_handle(path)?;
            let result = (|| -> Result<Vec<(u8, u8, u32)>> {
                let mut entries = Vec::new();
                for index in 0..u32::from((*dacl).AceCount) {
                    let mut entry = std::ptr::null_mut();
                    if GetAce(dacl, index, &mut entry) == 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let header = &*entry.cast::<ACE_HEADER>();
                    if header.AceType != 0 && header.AceType != 1 {
                        continue;
                    }
                    let ace = &*entry.cast::<ACCESS_ALLOWED_ACE>();
                    if EqualSid(
                        std::ptr::addr_of!(ace.SidStart).cast_mut().cast(),
                        sid.as_ptr(),
                    ) != 0
                    {
                        let mask = (*entry.cast::<ACCESS_DENIED_ACE>()).Mask;
                        entries.push((header.AceType, header.AceFlags, mask));
                    }
                }
                entries.sort();
                Ok(entries)
            })();
            LocalFree(descriptor);
            result
        }
    }

    #[test]
    fn persistent_read_revocation_preserves_grants_write_denies_and_other_principals() -> Result<()>
    {
        use crate::acl::add_deny_read_ace;
        use crate::acl::add_deny_write_ace;
        use crate::acl::ensure_allow_mask_aces;
        use windows_sys::Win32::Storage::FileSystem::DELETE;
        use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
        use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
        for write_first in [true, false] {
            let home = tempfile::tempdir()?;
            let other_home = tempfile::tempdir()?;
            let workspace = tempfile::tempdir()?;
            let secret = workspace.path().join("secret");
            std::fs::write(&secret, b"preserved contents")?;
            let principal = crate::cap::load_or_create_cap_sids(home.path())?.readonly;
            let sid = LocalSid::from_string(&principal)?;
            let other_principal = crate::cap::load_or_create_cap_sids(other_home.path())?.readonly;
            let other_sid = LocalSid::from_string(&other_principal)?;
            unsafe {
                assert!(ensure_allow_mask_aces(
                    &secret,
                    &[sid.as_ptr()],
                    FILE_GENERIC_READ
                )?);
                assert!(add_deny_read_ace(&secret, other_sid.as_ptr())?);
                if write_first {
                    assert!(add_deny_write_ace(&secret, sid.as_ptr())?);
                }
            }
            let other_before = native_entries_for_sid(&secret, &other_sid)?;
            let grants_before = native_entries_for_sid(&secret, &sid)?
                .into_iter()
                .filter(|entry| entry.0 == 0)
                .collect::<Vec<_>>();
            assert!(!grants_before.is_empty());
            unsafe {
                sync_persistent_deny_read_acls(
                    home.path(),
                    &principal,
                    &[secret.clone()],
                    sid.as_ptr(),
                )?;
                if !write_first {
                    assert!(add_deny_write_ace(&secret, sid.as_ptr())?);
                }
            }
            let with_read = native_entries_for_sid(&secret, &sid)?;
            assert!(
                with_read
                    .iter()
                    .any(|entry| entry.0 == 1 && entry.2 & FILE_READ_DATA != 0)
            );
            assert!(
                with_read
                    .iter()
                    .any(|entry| entry.0 == 1 && entry.2 & FILE_WRITE_DATA != 0)
            );
            unsafe {
                sync_persistent_deny_read_acls(home.path(), &principal, &[], sid.as_ptr())?;
            }
            let after = native_entries_for_sid(&secret, &sid)?;
            assert!(
                !after
                    .iter()
                    .any(|entry| entry.0 == 1 && entry.2 & FILE_READ_DATA != 0)
            );
            assert_eq!(
                after
                    .iter()
                    .filter(|entry| entry.0 == 1)
                    .fold(0, |mask, entry| mask | entry.2),
                FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD,
                "all existing write restrictions, including delete, must survive"
            );
            assert_eq!(
                after
                    .into_iter()
                    .filter(|entry| entry.0 == 0)
                    .collect::<Vec<_>>(),
                grants_before
            );
            assert_eq!(native_entries_for_sid(&secret, &other_sid)?, other_before);
            assert!(
                !load_state(&sandbox_dir(home.path()).join(DENY_READ_ACL_STATE_FILE))?
                    .principals
                    .contains_key(&principal)
            );
            assert_eq!(std::fs::read(&secret)?, b"preserved contents");
        }
        Ok(())
    }

    #[test]
    fn custom_deny_mask_failure_retains_tracking_and_native_permissions() -> Result<()> {
        use windows_sys::Win32::Security::ACCESS_DENIED_ACE;
        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
        use windows_sys::Win32::Security::EqualSid;
        use windows_sys::Win32::Security::GetAce;
        use windows_sys::Win32::Storage::FileSystem::FILE_EXECUTE;
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let secret = workspace.path().join("secret");
        std::fs::write(&secret, b"unchanged")?;
        let principal = crate::cap::load_or_create_cap_sids(home.path())?.readonly;
        let sid = LocalSid::from_string(&principal)?;
        unsafe {
            sync_persistent_deny_read_acls(
                home.path(),
                &principal,
                &[secret.clone()],
                sid.as_ptr(),
            )?;
            // External native policy edits may combine unrelated rights. The
            // normal reconciler must not guess which custom permissions it owns.
            let (dacl, descriptor) = fetch_dacl_handle(&secret)?;
            let mut changed = false;
            for index in 0..u32::from((*dacl).AceCount) {
                let mut entry = std::ptr::null_mut();
                assert_ne!(GetAce(dacl, index, &mut entry), 0);
                let ace = &mut *entry.cast::<ACCESS_DENIED_ACE>();
                if ace.Header.AceType == 1
                    && EqualSid(std::ptr::addr_of_mut!(ace.SidStart).cast(), sid.as_ptr()) != 0
                {
                    ace.Mask |= FILE_EXECUTE;
                    changed = true;
                }
            }
            assert!(changed);
            let code = SetNamedSecurityInfoW(
                crate::winutil::to_wide(&secret).as_ptr().cast_mut(),
                1,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            );
            LocalFree(descriptor);
            assert_eq!(code, 0);
        }
        let before = native_entries_for_sid(&secret, &sid)?;
        for _ in 0..2 {
            let error = unsafe {
                sync_persistent_deny_read_acls(home.path(), &principal, &[], sid.as_ptr())
            }
            .expect_err("custom deny rights must remain tracked on refusal");
            assert!(format!("{error:#}").contains("cannot safely revoke custom deny-read mask"));
            assert_eq!(native_entries_for_sid(&secret, &sid)?, before);
            assert_eq!(
                load_state(&sandbox_dir(home.path()).join(DENY_READ_ACL_STATE_FILE))?
                    .principals
                    .get(&principal),
                Some(&vec![secret.clone()])
            );
        }
        assert_eq!(std::fs::read(&secret)?, b"unchanged");
        Ok(())
    }

    #[test]
    fn native_deny_write_failure_preserves_retry_journal_and_rolls_back_partial_application()
    -> Result<()> {
        use crate::acl::native_deny_write_test;
        use crate::acl::native_deny_write_test::Stage;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        for (stage, operation) in [
            (Stage::Entries, "SetEntriesInAclW"),
            (Stage::Security, "SetNamedSecurityInfoW"),
        ] {
            let home = tempfile::tempdir()?;
            let workspace = tempfile::tempdir()?;
            let old = workspace.path().join("old");
            let first = workspace.path().join("first");
            let failing = workspace.path().join("failing");
            for path in [&old, &first, &failing] {
                std::fs::write(path, b"preserved bytes")?;
            }
            let principal = crate::cap::load_or_create_cap_sids(home.path())?.readonly;
            let sid = LocalSid::from_string(&principal)?;
            unsafe {
                sync_persistent_deny_read_acls(
                    home.path(),
                    &principal,
                    &[old.clone()],
                    sid.as_ptr(),
                )?;
            }
            let desired = [first.clone(), failing.clone()];
            let error = native_deny_write_test::with_error(
                &failing,
                stage,
                ERROR_ACCESS_DENIED,
                || unsafe {
                    sync_persistent_deny_read_acls(home.path(), &principal, &desired, sid.as_ptr())
                },
            )
            .expect_err("a required native ACL write failure must fail preparation");
            assert!(format!("{error:#}").contains(operation), "{error:#}");
            assert!(error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32))
            }));
            assert!(
                has_deny(&old, &sid)?,
                "old policy must not be revoked after failed application"
            );
            assert!(
                !has_deny(&first, &sid)?,
                "earlier new deny must be rolled back"
            );
            assert!(
                !has_deny(&failing, &sid)?,
                "failed OS write must not report an installed deny"
            );
            let state_path = sandbox_dir(home.path()).join(DENY_READ_ACL_STATE_FILE);
            assert_eq!(
                load_state(&state_path)?.principals.get(&principal),
                Some(&vec![old.clone(), first.clone(), failing.clone()])
            );
            unsafe {
                sync_persistent_deny_read_acls(home.path(), &principal, &desired, sid.as_ptr())?;
            }
            assert!(!has_deny(&old, &sid)?);
            assert!(has_deny(&first, &sid)?);
            assert!(has_deny(&failing, &sid)?);
            assert_eq!(
                load_state(&state_path)?.principals.get(&principal),
                Some(&desired.to_vec())
            );
            unsafe {
                sync_persistent_deny_read_acls(home.path(), &principal, &[], sid.as_ptr())?;
            }
            for path in [&old, &first, &failing] {
                assert!(!has_deny(path, &sid)?);
                assert_eq!(std::fs::read(path)?, b"preserved bytes");
            }
            assert!(!load_state(&state_path)?.principals.contains_key(&principal));
        }
        Ok(())
    }
}
