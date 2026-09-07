use crate::path_normalization::canonical_path_key;
use crate::path_normalization::canonicalize_path;
use anyhow::Context;
use anyhow::Result;
use rand::RngCore;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CapSids {
    pub workspace: String,
    pub readonly: String,
    /// Per-workspace capability SIDs keyed by canonicalized CWD string.
    ///
    /// This is used to isolate workspaces from other workspace sandbox writes and to
    /// apply per-workspace denies (e.g. protect `CWD/.codex`)
    /// without permanently affecting other workspaces.
    #[serde(default)]
    pub workspace_by_cwd: HashMap<String, String>,
    /// Per-write-root capability SIDs keyed by canonicalized write-root path.
    ///
    /// These are included in a workspace-write token only when the root is
    /// currently allowed, so stale ACLs from earlier extra roots do not expand
    /// later workspace sandboxes.
    #[serde(default)]
    pub writable_root_by_path: HashMap<String, String>,
}

#[derive(Clone)]
struct CachedCapSids {
    caps: CapSids,
    #[cfg(test)]
    disk_load_count: usize,
}

static CAP_SIDS_CACHE: OnceLock<Mutex<HashMap<String, CachedCapSids>>> = OnceLock::new();

fn cap_sids_cache() -> &'static Mutex<HashMap<String, CachedCapSids>> {
    CAP_SIDS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn cap_sid_file(codex_home: &Path) -> PathBuf {
    codex_home.join("cap_sid")
}

fn make_random_cap_sid_string() -> String {
    let mut rng = SmallRng::from_os_rng();
    let a = rng.next_u32();
    let b = rng.next_u32();
    let c = rng.next_u32();
    let d = rng.next_u32();
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

fn persist_caps(path: &Path, caps: &CapSids) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        anyhow::anyhow!("cap sid file {} has no parent directory", path.display())
    })?;
    fs::create_dir_all(dir).with_context(|| format!("create cap sid dir {}", dir.display()))?;
    let json = serde_json::to_string(caps)?;
    // Replace atomically. A crash or a concurrent reader must never observe a
    // truncated record: a damaged file fails closed instead of regenerating SIDs.
    let mut staged = tempfile::Builder::new()
        .prefix(".cap_sid-")
        .tempfile_in(dir)
        .with_context(|| format!("stage cap sid file in {}", dir.display()))?;
    staged
        .write_all(json.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .with_context(|| format!("write staged cap sid file for {}", path.display()))?;
    staged
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace cap sid file {}", path.display()))?;
    Ok(())
}

/// The legacy pre-JSON file held exactly one generated capability SID.
fn is_capability_sid_string(value: &str) -> bool {
    value.strip_prefix("S-1-5-21-").is_some_and(|rest| {
        let components: Vec<&str> = rest.split('-').collect();
        components.len() == 4
            && components
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn load_or_create_cap_sids_from_disk(codex_home: &Path) -> Result<CapSids> {
    let path = cap_sid_file(codex_home);
    if path.exists() {
        let txt = fs::read_to_string(&path)
            .with_context(|| format!("read cap sid file {}", path.display()))?;
        let t = txt.trim();
        if t.starts_with('{') && t.ends_with('}') {
            // Regenerating here would silently orphan every ACL installed for the
            // previous identities, including the per-workspace deny entries that
            // protect `.codex`; a damaged record must stop the sandbox instead.
            return serde_json::from_str::<CapSids>(t).with_context(|| {
                format!(
                    "cap sid file {} is not a valid capability SID record; refusing to regenerate capability SIDs",
                    path.display()
                )
            });
        } else if !t.is_empty() {
            if !is_capability_sid_string(t) {
                anyhow::bail!(
                    "cap sid file {} is neither a JSON record nor a capability SID; refusing to regenerate capability SIDs",
                    path.display()
                );
            }
            let caps = CapSids {
                workspace: t.to_string(),
                readonly: make_random_cap_sid_string(),
                workspace_by_cwd: HashMap::new(),
                writable_root_by_path: HashMap::new(),
            };
            persist_caps(&path, &caps)?;
            return Ok(caps);
        }
    }
    let caps = CapSids {
        workspace: make_random_cap_sid_string(),
        readonly: make_random_cap_sid_string(),
        workspace_by_cwd: HashMap::new(),
        writable_root_by_path: HashMap::new(),
    };
    persist_caps(&path, &caps)?;
    Ok(caps)
}

fn cached_cap_sids<'a>(
    codex_home: &Path,
    cache: &'a mut HashMap<String, CachedCapSids>,
) -> Result<&'a mut CachedCapSids> {
    let key = canonical_path_key(&cap_sid_file(codex_home));
    if !cache.contains_key(&key) {
        let caps = load_or_create_cap_sids_from_disk(codex_home)?;
        cache.insert(
            key.clone(),
            CachedCapSids {
                caps,
                #[cfg(test)]
                disk_load_count: 1,
            },
        );
    }
    cache
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("capability SID cache insertion failed"))
}

/// Refreshes this process's capability SID cache after the setup helper has
/// updated the shared `cap_sid` file in a separate process.
///
/// Per-root capability SIDs must match the ACLs installed by that helper. A
/// long-lived parent process cannot keep using the cache it populated before a
/// newly allowed root was added on disk.
pub(crate) fn refresh_cap_sids_cache_from_disk(codex_home: &Path) -> Result<()> {
    let key = canonical_path_key(&cap_sid_file(codex_home));
    let mut cache = cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let caps = load_or_create_cap_sids_from_disk(codex_home)?;
    #[cfg(test)]
    let disk_load_count = cache
        .get(&key)
        .map_or(1, |cached| cached.disk_load_count + 1);
    cache.insert(
        key,
        CachedCapSids {
            caps,
            #[cfg(test)]
            disk_load_count,
        },
    );
    Ok(())
}

/// Loads the process-stable capability SID set once per Codex home.
///
/// In-process mutations use the helpers in this module and update the cached
/// value while holding the same lock. The Windows setup helper runs in a
/// separate process, so its caller must refresh this cache after the helper
/// exits successfully. This avoids reopening and parsing the SID file for every
/// sandboxed command without changing SID persistence.
pub fn load_or_create_cap_sids(codex_home: &Path) -> Result<CapSids> {
    let mut cache = cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(cached_cap_sids(codex_home, &mut cache)?.caps.clone())
}

/// Returns the workspace-specific capability SID for `cwd`, creating and persisting it if missing.
pub fn workspace_cap_sid_for_cwd(codex_home: &Path, cwd: &Path) -> Result<String> {
    workspace_cap_sid_for_key(codex_home, canonical_path_key(cwd))
}

fn workspace_cap_sid_for_key(codex_home: &Path, key: String) -> Result<String> {
    let path = cap_sid_file(codex_home);
    let mut cache = cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = cached_cap_sids(codex_home, &mut cache)?;
    if let Some(sid) = cached.caps.workspace_by_cwd.get(&key) {
        return Ok(sid.clone());
    }
    let sid = make_random_cap_sid_string();
    cached.caps.workspace_by_cwd.insert(key, sid.clone());
    persist_caps(&path, &cached.caps)?;
    Ok(sid)
}

/// Returns the capability SID for an additional writable root, creating and persisting it if missing.
#[cfg(test)]
pub fn writable_root_cap_sid_for_path(codex_home: &Path, root: &Path) -> Result<String> {
    writable_root_cap_sid_for_key(codex_home, canonical_path_key(root))
}

fn writable_root_cap_sid_for_key(codex_home: &Path, key: String) -> Result<String> {
    let path = cap_sid_file(codex_home);
    let mut cache = cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = cached_cap_sids(codex_home, &mut cache)?;
    if let Some(sid) = cached.caps.writable_root_by_path.get(&key) {
        return Ok(sid.clone());
    }
    let sid = make_random_cap_sid_string();
    cached.caps.writable_root_by_path.insert(key, sid.clone());
    persist_caps(&path, &cached.caps)?;
    Ok(sid)
}

#[cfg(test)]
fn cap_sid_disk_load_count(codex_home: &Path) -> usize {
    let key = canonical_path_key(&cap_sid_file(codex_home));
    cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .map_or(0, |cached| cached.disk_load_count)
}

pub fn workspace_write_cap_sid_for_root(
    codex_home: &Path,
    cwd: &Path,
    root: &Path,
) -> Result<String> {
    workspace_write_cap_sid_for_root_keys(
        codex_home,
        &canonical_path_key(cwd),
        canonical_path_key(root),
    )
}

pub(crate) fn workspace_write_cap_sid_for_root_keys(
    codex_home: &Path,
    cwd_key: &str,
    root_key: String,
) -> Result<String> {
    if root_key == cwd_key {
        workspace_cap_sid_for_key(codex_home, cwd_key.to_string())
    } else {
        writable_root_cap_sid_for_key(codex_home, root_key)
    }
}

pub fn workspace_write_root_contains_path(root: &Path, path: &Path) -> bool {
    canonicalize_path(path).starts_with(canonicalize_path(root))
}

pub fn workspace_write_root_overlaps_path(root: &Path, path: &Path) -> bool {
    workspace_write_root_contains_path(root, path) || workspace_write_root_contains_path(path, root)
}

pub fn workspace_write_root_specificity(root: &Path) -> usize {
    canonicalize_path(root).components().count()
}

#[cfg(test)]
mod tests {
    use super::canonical_path_key;
    use super::cap_sid_disk_load_count;
    use super::cap_sid_file;
    use super::load_or_create_cap_sids;
    use super::load_or_create_cap_sids_from_disk;
    use super::make_random_cap_sid_string;
    use super::persist_caps;
    use super::refresh_cap_sids_cache_from_disk;
    use super::workspace_cap_sid_for_cwd;
    use super::workspace_write_cap_sid_for_root;
    use super::workspace_write_cap_sid_for_root_keys;
    use super::writable_root_cap_sid_for_path;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn repeated_cap_sid_loads_reuse_the_process_cache() {
        let home = TempDir::new().expect("temp dir");

        let first = load_or_create_cap_sids(home.path()).expect("first cap SID load");
        let second = load_or_create_cap_sids(home.path()).expect("cached cap SID load");

        assert_eq!(first.workspace, second.workspace);
        assert_eq!(first.readonly, second.readonly);
        assert_eq!(cap_sid_disk_load_count(home.path()), 1);
    }

    #[test]
    fn external_cap_sid_rewrite_refreshes_process_cache() {
        let temp = TempDir::new().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        let workspace = temp.path().join("workspace");
        let report_root = temp.path().join("report-root");
        std::fs::create_dir_all(&codex_home).expect("create codex home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&report_root).expect("create report root");

        load_or_create_cap_sids(&codex_home).expect("warm parent process cache");
        let helper_sid = make_random_cap_sid_string();
        let mut helper_caps = load_or_create_cap_sids_from_disk(&codex_home)
            .expect("load setup helper capability SIDs");
        helper_caps
            .writable_root_by_path
            .insert(canonical_path_key(&report_root), helper_sid.clone());
        persist_caps(&cap_sid_file(&codex_home), &helper_caps)
            .expect("persist setup helper capability SIDs");

        refresh_cap_sids_cache_from_disk(&codex_home)
            .expect("refresh parent process cache after setup helper");

        assert_eq!(
            workspace_write_cap_sid_for_root(&codex_home, &workspace, &report_root)
                .expect("look up refreshed report-root SID"),
            helper_sid
        );
        assert_eq!(cap_sid_disk_load_count(&codex_home), 2);
    }

    #[test]
    fn damaged_json_cap_sid_file_fails_closed_and_is_preserved() {
        let home = TempDir::new().expect("temp dir");
        let path = cap_sid_file(home.path());
        let damaged = br#"{"workspace": 1, "readonly": "S-1-5-21-1-2-3-4"}"#;
        std::fs::write(&path, damaged).expect("write damaged record");

        let error = load_or_create_cap_sids(home.path())
            .expect_err("a damaged record must not regenerate capability SIDs");

        assert!(
            error.to_string().contains("refusing to regenerate"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).expect("preserved bytes"), damaged);
    }

    #[test]
    fn truncated_cap_sid_file_fails_closed_and_is_preserved() {
        let home = TempDir::new().expect("temp dir");
        let path = cap_sid_file(home.path());
        // A write interrupted before the closing brace used to be treated as a
        // legacy plain-text SID and silently replaced by a fresh record.
        let truncated = br#"{"workspace":"S-1-5-21-1-2-3-4","readonly":"S-1-5-21-5-6"#;
        std::fs::write(&path, truncated).expect("write truncated record");

        let error = load_or_create_cap_sids(home.path())
            .expect_err("a truncated record must not regenerate capability SIDs");

        assert!(
            error.to_string().contains("refusing to regenerate"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).expect("preserved bytes"), truncated);
    }

    #[test]
    fn legacy_plain_text_cap_sid_file_still_migrates() {
        let home = TempDir::new().expect("temp dir");
        let path = cap_sid_file(home.path());
        std::fs::write(&path, "S-1-5-21-1-2-3-4\n").expect("write legacy record");

        let caps = load_or_create_cap_sids(home.path()).expect("migrate legacy record");

        assert_eq!(caps.workspace, "S-1-5-21-1-2-3-4");
        let migrated: super::CapSids =
            serde_json::from_slice(&std::fs::read(&path).expect("migrated bytes")).expect("json");
        assert_eq!(migrated.workspace, caps.workspace);
        assert_eq!(migrated.readonly, caps.readonly);
    }

    #[test]
    fn persist_caps_replaces_the_file_without_staging_litter() {
        let home = TempDir::new().expect("temp dir");
        let path = cap_sid_file(home.path());
        let first = load_or_create_cap_sids(home.path()).expect("initial record");
        let extra_sid = workspace_write_cap_sid_for_root_keys(
            home.path(),
            "c:\\workspace",
            "c:\\extra".to_string(),
        )
        .expect("persist new root through the runtime entrypoint");

        let entries: Vec<String> = std::fs::read_dir(home.path())
            .expect("home listing")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(entries, vec!["cap_sid".to_string()]);
        let reloaded: super::CapSids =
            serde_json::from_slice(&std::fs::read(&path).expect("record bytes")).expect("json");
        assert_eq!(reloaded.writable_root_by_path.len(), 1);
        assert_eq!(reloaded.writable_root_by_path["c:\\extra"], extra_sid);
        assert_eq!(reloaded.workspace, first.workspace);
        assert_eq!(reloaded.readonly, first.readonly);
        assert_eq!(reloaded.workspace_by_cwd, first.workspace_by_cwd);
        let cached = load_or_create_cap_sids(home.path()).expect("runtime cache after write");
        assert_eq!(cached.workspace, reloaded.workspace);
        assert_eq!(cached.readonly, reloaded.readonly);
        assert_eq!(cached.workspace_by_cwd, reloaded.workspace_by_cwd);
        assert_eq!(cached.writable_root_by_path, reloaded.writable_root_by_path);
    }

    #[test]
    fn generated_cap_sid_matches_windows_capability_shape() {
        let sid = make_random_cap_sid_string();
        let components = sid
            .strip_prefix("S-1-5-21-")
            .expect("capability SID prefix")
            .split('-')
            .collect::<Vec<_>>();

        assert_eq!(components.len(), 4);
        assert!(components.iter().all(|value| value.parse::<u32>().is_ok()));
    }

    #[test]
    fn equivalent_cwd_spellings_share_workspace_sid_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex home");

        let workspace = temp.path().join("WorkspaceRoot");
        std::fs::create_dir_all(&workspace).expect("create workspace root");

        let canonical = dunce::canonicalize(&workspace).expect("canonical workspace root");
        let alt_spelling = PathBuf::from(
            canonical
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_uppercase(),
        );

        let first_sid =
            workspace_cap_sid_for_cwd(&codex_home, canonical.as_path()).expect("first sid");
        let second_sid =
            workspace_cap_sid_for_cwd(&codex_home, alt_spelling.as_path()).expect("second sid");

        assert_eq!(first_sid, second_sid);

        let caps = load_or_create_cap_sids(&codex_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd.len(), 1);
    }

    #[test]
    fn write_roots_get_path_scoped_sids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex home");

        let workspace = temp.path().join("workspace");
        let extra_root = temp.path().join("extra-root");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&extra_root).expect("create extra root");

        let workspace_sid = workspace_write_cap_sid_for_root(&codex_home, &workspace, &workspace)
            .expect("workspace sid");
        let extra_sid = workspace_write_cap_sid_for_root(&codex_home, &workspace, &extra_root)
            .expect("extra root sid");

        assert_ne!(workspace_sid, extra_sid);
        assert_eq!(
            extra_sid,
            writable_root_cap_sid_for_path(&codex_home, &extra_root).expect("extra root sid again")
        );

        let caps = load_or_create_cap_sids(&codex_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd.len(), 1);
        assert_eq!(caps.writable_root_by_path.len(), 1);
    }

    #[test]
    fn precomputed_write_root_keys_are_reused_for_sid_lookup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex home");

        let workspace_sid = workspace_write_cap_sid_for_root_keys(
            &codex_home,
            "c:\\workspace",
            "c:\\workspace".to_string(),
        )
        .expect("workspace sid");
        let extra_sid = workspace_write_cap_sid_for_root_keys(
            &codex_home,
            "c:\\workspace",
            "c:\\extra".to_string(),
        )
        .expect("extra-root sid");

        let caps = load_or_create_cap_sids(&codex_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd["c:\\workspace"], workspace_sid);
        assert_eq!(caps.writable_root_by_path["c:\\extra"], extra_sid);
    }
}
