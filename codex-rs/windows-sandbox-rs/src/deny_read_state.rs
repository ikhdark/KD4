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
    unsafe { sync_deny_read_acls(codex_home, principal_sid, desired_paths, psid, true) }
}

/// Shared logon principals can still belong to surviving descendants. Refuse
/// policy relaxation without a principal-wide quiescence proof.
///
/// # Safety
/// Caller must retain a valid SID matching principal_sid.
pub unsafe fn sync_shared_principal_deny_read_acls(
    codex_home: &Path, principal_sid: &str, desired_paths: &[PathBuf], psid: *mut c_void,
) -> Result<Vec<PathBuf>> {
    unsafe { sync_deny_read_acls(codex_home, principal_sid, desired_paths, psid, false) }
}

unsafe fn sync_deny_read_acls(
    codex_home: &Path, principal_sid: &str, desired_paths: &[PathBuf], psid: *mut c_void, allow_relaxation: bool,
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
    if !allow_relaxation {
        let desired = planned_paths.iter().map(|path| lexical_path_key(path)).collect::<HashSet<_>>();
        anyhow::ensure!(previous_paths.iter().all(|path| desired.contains(&lexical_path_key(path))),
            "cannot relax shared sandbox read-deny policy while principal quiescence is unproven");
    }
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

    // SAFETY: The function's caller guarantees psid is a valid SID matching principal_sid; this
    // synchronous helper borrows it without taking ownership.
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
            // SAFETY: The caller-guaranteed SID remains live through this synchronous revocation;
            // path is an owned retained journal entry.
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

