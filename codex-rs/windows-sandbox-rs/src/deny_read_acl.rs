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

fn push_planned_path(planned: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    if seen.insert(lexical_path_key(&path)) {
        planned.push(path);
    }
}

pub(crate) fn lexical_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
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
        let result = (|| -> Result<bool> {
            if !path.exists() {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create deny-read path {}", path.display()))?;
            }
            add_deny_read_ace(&path, psid)
                .with_context(|| format!("apply deny-read ACE to {}", path.display()))
        })();
        let added = match result {
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
        if added {
            added_in_this_call.push(path.clone());
        }
        push_planned_path(&mut applied, &mut seen, path);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::plan_deny_read_acl_paths;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn plan_preserves_missing_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("future-secret.env");

        assert_eq!(
            plan_deny_read_acl_paths(std::slice::from_ref(&missing)),
            vec![missing]
        );
    }

    #[test]
    fn plan_includes_existing_canonical_targets() {
        let tmp = TempDir::new().expect("tempdir");
        let existing = tmp.path().join("secret.env");
        std::fs::write(&existing, "secret").expect("write secret");

        let planned: HashSet<PathBuf> = plan_deny_read_acl_paths(std::slice::from_ref(&existing))
            .into_iter()
            .collect();
        let expected: HashSet<PathBuf> = [
            existing.clone(),
            dunce::canonicalize(&existing).expect("canonical path"),
        ]
        .into_iter()
        .collect();

        assert_eq!(planned, expected);
    }

    #[test]
    fn failed_deny_read_application_revokes_only_new_denies() -> anyhow::Result<()> {
        let home = TempDir::new()?;
        let workspace = TempDir::new()?;
        let existing = workspace.path().join("existing");
        let added = workspace.path().join("added");
        let blocker = workspace.path().join("file-not-directory");
        for path in [&existing, &added, &blocker] {
            std::fs::write(path, b"original bytes")?;
        }
        let principal = crate::cap::load_or_create_cap_sids(home.path())?.readonly;
        let sid = crate::token::LocalSid::from_string(&principal)?;
        // SAFETY: sid owns the valid SID throughout the ACL updates and comparisons. Each fetched
        // descriptor keeps its DACL live until the comparison finishes and is then released
        // once.
        unsafe {
            assert!(crate::acl::add_deny_read_ace(&existing, sid.as_ptr())?);
            let error = super::apply_deny_read_acls(
                &[existing.clone(), added.clone(), blocker.join("child")],
                sid.as_ptr(),
            )
            .expect_err("a file parent cannot materialize a deny directory");
            assert!(format!("{error:#}").contains("create deny-read path"));
            for (path, expected) in [(&existing, true), (&added, false)] {
                let (dacl, descriptor) = crate::acl::fetch_dacl_handle(path)?;
                let denied = crate::acl::dacl_has_read_deny_for_sid(dacl, sid.as_ptr());
                windows_sys::Win32::Foundation::LocalFree(descriptor);
                assert_eq!(denied, expected, "{}", path.display());
                assert_eq!(std::fs::read(path)?, b"original bytes");
            }
        }
        assert_eq!(std::fs::read(&blocker)?, b"original bytes");
        assert!(!blocker.join("child").exists());
        Ok(())
    }
}
