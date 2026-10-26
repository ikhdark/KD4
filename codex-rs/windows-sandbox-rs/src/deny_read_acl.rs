use crate::acl::add_deny_read_ace;
use crate::acl::revoke_deny_read_ace;
use crate::path_normalization::canonicalize_path;
use anyhow::Context;
use anyhow::Result;
use std::collections::HashSet;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;

/// Build the exact ACL paths that should receive a deny-read ACE.
///
/// We keep both the lexical policy path and, when it already exists, the
/// canonical target. The lexical path covers the path users configured and lets
/// missing exact denies be materialized later; the canonical path also covers
/// an existing reparse-point target so a sandbox cannot read the same object
/// through the resolved location.
pub fn plan_deny_read_acl_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut planned = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        push_planned_path(&mut planned, &mut seen, path.to_path_buf());
        if path.exists() {
            push_planned_path(&mut planned, &mut seen, canonicalize_path(path));
        }
    }
    planned
}

fn push_planned_path(planned: &mut Vec<PathBuf>, seen: &mut HashSet<std::ffi::OsString>, path: PathBuf) {
    if seen.insert(lexical_path_key(&path)) {
        planned.push(path);
    }
}

pub(crate) fn lexical_path_key(path: &Path) -> std::ffi::OsString {
    // Security policy paths may be case-sensitive and need not be Unicode.
    // Duplicate ACL checks are safe; losing a distinct path is not.
    path.as_os_str().to_owned()
}

/// Applies deny-read ACEs to explicit paths. Missing paths are materialized as
/// directories before the ACE is applied so a sandboxed command cannot create a
/// previously absent denied path and then read from it in the same run.
/// If any path fails, deny ACEs applied by this call are revoked before the
/// error is returned so a one-shot sandbox run does not leave partial state.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub unsafe fn apply_deny_read_acls(paths: &[PathBuf], psid: *mut c_void) -> Result<Vec<PathBuf>> {
    let planned = plan_deny_read_acl_paths(paths);
    // SAFETY: The caller's valid-SID requirement is forwarded unchanged to the synchronous planned-
    // path helper.
    unsafe { apply_planned_deny_read_acls(planned, psid) }
}

/// Apply an already resolved plan without introducing new canonical targets.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub(crate) unsafe fn apply_planned_deny_read_acls(
    planned: Vec<PathBuf>,
    psid: *mut c_void,
) -> Result<Vec<PathBuf>> {
    let mut applied = Vec::new();
    let mut seen = HashSet::new();
    let mut added_in_this_call: Vec<PathBuf> = Vec::new();
    for path in planned {
        let result = (|| -> Result<(bool, bool)> {
            if !path.exists() {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create deny-read path {}", path.display()))?;
            }
            let (dacl, descriptor) = crate::acl::fetch_dacl_handle(&path)?;
            let preexisting = crate::acl::dacl_has_any_read_deny_for_sid(dacl, psid);
            if !descriptor.is_null() { windows_sys::Win32::Foundation::LocalFree(descriptor); }
            let added = add_deny_read_ace(&path, psid)
                .with_context(|| format!("apply deny-read ACE to {}", path.display()))?;
            Ok((added, preexisting))
        })();
        let (added, preexisting) = match result {
            Ok(added) => added,
            Err(err) => {
                let mut rollback_errors = Vec::new();
                for added_path in &added_in_this_call {
                    if let Err(rollback_error) = revoke_deny_read_ace(added_path, psid) {
                        rollback_errors.push(format!("{}: {rollback_error}", added_path.display()));
                    }
                }
                if !rollback_errors.is_empty() {
                    return Err(err.context(format!(
                        "deny-read rollback also failed: {}",
                        rollback_errors.join("; ")
                    )));
                }
                return Err(err);
            }
        };
        // Retain repairs to preexisting protection if a later path fails.
        if added && !preexisting {
            added_in_this_call.push(path.clone());
        }
        push_planned_path(&mut applied, &mut seen, path);
    }
    Ok(applied)
}

