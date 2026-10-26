use crate::path_normalization::security_path_key as canonical_path_key;
use crate::path_normalization::canonicalize_path;
use anyhow::Context;
use anyhow::Result;
use rand::Rng;
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
}

static CAP_SIDS_CACHE: OnceLock<Mutex<HashMap<String, CachedCapSids>>> = OnceLock::new();

fn cap_sids_cache() -> &'static Mutex<HashMap<String, CachedCapSids>> {
    CAP_SIDS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn cap_sid_file(codex_home: &Path) -> PathBuf {
    codex_home.join("cap_sid")
}

fn make_random_cap_sid_string() -> String {
    let mut rng: SmallRng = rand::make_rng();
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

/// Serializes cap SID file loads that may write it across Codex and setup-helper processes.
///
/// Every writer holds this lock from load through rename, so a concurrent writer cannot replace
/// the file with a copy that lacks another process's newly granted SID.
fn lock_cap_sid_file(codex_home: &Path) -> Result<fs::File> {
    fs::create_dir_all(codex_home)
        .with_context(|| format!("create cap sid dir {}", codex_home.display()))?;
    let path = cap_sid_file(codex_home).with_extension("lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open cap sid lock {}", path.display()))?;
    lock.lock()
        .with_context(|| format!("lock cap sid file {}", path.display()))?;
    Ok(lock)
}

fn load_or_create_cap_sids_from_disk(codex_home: &Path) -> Result<CapSids> {
    let path = cap_sid_file(codex_home);
    match fs::read_to_string(&path) {
        Ok(txt) => {
        let t = txt.trim();
        if t.starts_with('{') {
            let caps: CapSids = serde_json::from_str(t)
                .with_context(|| format!("parse cap sid file {}", path.display()))?;
            validate_caps(&caps)?;
            return Ok(caps);
        } else {
            // Only a valid legacy single SID may be upgraded. Existing corrupt
            // state must never rotate identities already bound to durable ACLs.
            validate_cap_sid(t).context("invalid legacy capability SID")?;
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
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("read cap sid file {}", path.display())),
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

fn validate_caps(caps: &CapSids) -> Result<()> {
    for sid in std::iter::once(&caps.workspace)
        .chain(std::iter::once(&caps.readonly))
        .chain(caps.workspace_by_cwd.values())
        .chain(caps.writable_root_by_path.values())
    {
        validate_cap_sid(sid).context("invalid persisted capability SID")?;
    }
    Ok(())
}

fn validate_cap_sid(sid: &str) -> Result<()> {
    let components = sid.strip_prefix("S-1-5-21-").context("unsupported capability SID shape")?
        .split('-').collect::<Vec<_>>();
    anyhow::ensure!(components.len() == 4 && components.iter().all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) && part.parse::<u32>().is_ok()), "invalid capability SID components");
    crate::token::LocalSid::from_string(sid)?;
    Ok(())
}

fn cached_cap_sids<'a>(
    codex_home: &Path,
    cache: &'a mut HashMap<String, CachedCapSids>,
) -> Result<&'a mut CachedCapSids> {
    let key = canonical_path_key(&cap_sid_file(codex_home));
    if !cache.contains_key(&key) {
        let _lock = lock_cap_sid_file(codex_home)?;
        let caps = load_or_create_cap_sids_from_disk(codex_home)?;
        cache.insert(key.clone(), CachedCapSids { caps });
    }
    cache
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("capability SID cache insertion failed"))
}

/// Loads the process-stable capability SID set once per Codex home.
///
/// Keyed SIDs are only ever added, so cached hits stay valid. Other processes (the setup helper and
/// concurrent Codex instances) may add keyed SIDs after this cache was loaded; keyed lookups
/// therefore consult the file on a miss before minting a new SID.
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
    keyed_cap_sid(codex_home, key, |caps| &mut caps.workspace_by_cwd)
}


fn writable_root_cap_sid_for_key(codex_home: &Path, key: String) -> Result<String> {
    keyed_cap_sid(codex_home, key, |caps| &mut caps.writable_root_by_path)
}

fn keyed_cap_sid(
    codex_home: &Path,
    key: String,
    entries: fn(&mut CapSids) -> &mut HashMap<String, String>,
) -> Result<String> {
    let path = cap_sid_file(codex_home);
    let mut cache = cap_sids_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = cached_cap_sids(codex_home, &mut cache)?;
    if let Some(sid) = entries(&mut cached.caps).get(&key) {
        return Ok(sid.clone());
    }
    // The elevated setup refresh persists SIDs for new write roots and grants their ACLs before
    // this process resolves them. Minting from a stale cache would hand the command runner an
    // ungranted SID and drop the helper's entry from the file.
    let _lock = lock_cap_sid_file(codex_home)?;
    let mut latest = load_or_create_cap_sids_from_disk(codex_home)?;
    let sid = match entries(&mut latest).get(&key) {
        Some(sid) => sid.clone(),
        None => {
            let sid = make_random_cap_sid_string();
            entries(&mut latest).insert(key, sid.clone());
            persist_caps(&path, &latest)?;
            sid
        }
    };
    cached.caps = latest;
    Ok(sid)
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

