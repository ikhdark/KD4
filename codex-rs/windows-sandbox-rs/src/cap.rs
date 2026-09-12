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
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create cap sid dir {}", dir.display()))?;
    }
    let json = serde_json::to_string(caps)?;
    let parent = path.parent().context("cap sid file has no parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("stage cap sid file {}", path.display()))?;
    staged
        .write_all(json.as_bytes())
        .with_context(|| format!("write cap sid file {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("sync cap sid file {}", path.display()))?;
    let staged = staged.into_temp_path();
    fs::rename(&staged, path)
        .with_context(|| format!("replace cap sid file {}", path.display()))?;
    Ok(())
}

fn load_or_create_cap_sids_from_disk(codex_home: &Path) -> Result<CapSids> {
    let path = cap_sid_file(codex_home);
    if path.exists() {
        let txt = fs::read_to_string(&path)
            .with_context(|| format!("read cap sid file {}", path.display()))?;
        let t = txt.trim();
        if t.starts_with('{') && t.ends_with('}') {
            if let Ok(obj) = serde_json::from_str::<CapSids>(t) {
                return Ok(obj);
            }
        } else if !t.is_empty() {
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

/// Loads the process-stable capability SID set once per Codex home.
///
/// The file is mutated only through the helpers in this module, which update the
/// cached value while holding the same lock. This avoids reopening and parsing
/// the SID file for every sandboxed command without changing SID persistence.
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
    let mut updated = cached.caps.clone();
    updated.workspace_by_cwd.insert(key, sid.clone());
    persist_caps(&path, &updated)?;
    cached.caps = updated;
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
    let mut updated = cached.caps.clone();
    updated.writable_root_by_path.insert(key, sid.clone());
    persist_caps(&path, &updated)?;
    cached.caps = updated;
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
    use super::cap_sid_disk_load_count;
    use super::load_or_create_cap_sids;
    use super::make_random_cap_sid_string;
    use super::workspace_cap_sid_for_cwd;
    use super::workspace_write_cap_sid_for_root;
    use super::workspace_write_cap_sid_for_root_keys;
    use super::writable_root_cap_sid_for_path;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn failed_capability_persistence_does_not_publish_cache_entries() -> anyhow::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let home = TempDir::new()?;
        let roots = TempDir::new()?;
        let workspace = roots.path().join("workspace");
        let extra_root = roots.path().join("extra");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&extra_root)?;
        let original = load_or_create_cap_sids(home.path())?;
        let path = super::cap_sid_file(home.path());
        let original_bytes = std::fs::read(&path)?;
        // Deny writes and replacement while still allowing the test to read the target.
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)?;

        for root in [&workspace, &extra_root] {
            // A failed first attempt must not make the retry succeed from the cache.
            for _ in 0..2 {
                let error = workspace_write_cap_sid_for_root(home.path(), &workspace, root)
                    .expect_err("locked capability file must reject persistence");
                assert!(format!("{error:#}").contains("replace cap sid file"));
            }
        }
        let cached = load_or_create_cap_sids(home.path())?;
        assert_eq!(cached.workspace, original.workspace);
        assert_eq!(cached.readonly, original.readonly);
        assert!(cached.workspace_by_cwd.is_empty());
        assert!(cached.writable_root_by_path.is_empty());
        assert_eq!(std::fs::read(&path)?, original_bytes);
        let entries = std::fs::read_dir(home.path())?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(
            entries.len(),
            1,
            "failed writes must remove temporary files"
        );
        assert_eq!(entries[0].path(), path);
        drop(locked);

        let workspace_sid = workspace_write_cap_sid_for_root(home.path(), &workspace, &workspace)?;
        let extra_sid = workspace_write_cap_sid_for_root(home.path(), &workspace, &extra_root)?;
        assert_ne!(workspace_sid, extra_sid);
        // A distinct home forces the normal loader to consume persisted JSON, not this cache.
        let reopened_home = TempDir::new()?;
        std::fs::copy(&path, super::cap_sid_file(reopened_home.path()))?;
        assert_eq!(
            workspace_write_cap_sid_for_root(reopened_home.path(), &workspace, &workspace)?,
            workspace_sid
        );
        assert_eq!(
            workspace_write_cap_sid_for_root(reopened_home.path(), &workspace, &extra_root)?,
            extra_sid
        );
        let reopened = load_or_create_cap_sids(reopened_home.path())?;
        assert_eq!(reopened.workspace, original.workspace);
        assert_eq!(reopened.readonly, original.readonly);
        assert_eq!(reopened.workspace_by_cwd.len(), 1);
        assert_eq!(reopened.writable_root_by_path.len(), 1);
        Ok(())
    }

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
