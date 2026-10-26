use crate::acl::add_deny_write_ace;
use crate::acl::path_mask_allows;
use crate::cap::load_or_create_cap_sids;
use crate::cap::workspace_write_cap_sid_for_root;
use crate::cap::workspace_write_root_contains_path;
use crate::logging::debug_log;
use crate::logging::log_note;
use crate::path_normalization::canonical_path_key;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::setup::effective_write_roots_for_permissions;
use crate::setup::sandbox_dir;
use crate::token::LocalSid;
use crate::token::world_sid;
use anyhow::Result;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_EA;

// Preflight scan limits
const MAX_ITEMS_PER_DIR: i32 = 1000;
const AUDIT_TIME_LIMIT_SECS: i64 = 2;
const MAX_CHECKED_LIMIT: i32 = 50000;
// Case-insensitive suffixes (normalized to forward slashes) to skip during one-level child scan
const SKIP_DIR_SUFFIXES: &[&str] = &[
    "/windows/installer",
    "/windows/registration",
    "/programdata",
];

fn normalize_windows_path_for_display(path: impl AsRef<Path>) -> String {
    let path = dunce::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf());
    path.display().to_string().replace('/', "\\")
}

fn world_writable_warning_details_from_scan(
    scan: Result<WorldWritableScan>,
) -> Option<(Vec<String>, usize, bool)> {
    match scan {
        Ok(scan) if scan.paths.is_empty() && !scan.incomplete => None,
        Ok(scan) => {
            let paths = scan.paths;
            let paths = paths
                .iter()
                .map(normalize_windows_path_for_display)
                .collect::<Vec<_>>();
            let sample_paths = paths.iter().take(3).cloned().collect::<Vec<_>>();
            let extra_count = paths.len().saturating_sub(sample_paths.len());
            Some((sample_paths, extra_count, scan.incomplete))
        }
        Err(_) => Some((Vec::new(), 0, true)),
    }
}

pub fn world_writable_warning_details(
    codex_home: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
) -> Option<(Vec<String>, usize, bool)> {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let logs_base_dir = sandbox_dir(codex_home.as_ref());
    world_writable_warning_details_from_scan(audit_everyone_writable(
        cwd.as_ref(),
        &env_map,
        Some(&logs_base_dir),
    ))
}

fn unique_push(set: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>, p: PathBuf) {
    if let Ok(abs) = p.canonicalize()
        && set.insert(abs.clone())
    {
        out.push(abs);
    }
}

fn gather_candidates(cwd: &Path, env: &std::collections::HashMap<String, String>) -> Vec<PathBuf> {
    let mut set: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    // 1) CWD first (so immediate children get scanned early)
    unique_push(&mut set, &mut out, cwd.to_path_buf());
    // 2) TEMP/TMP next (often small, quick to scan)
    for k in ["TEMP", "TMP"] {
        if let Some(v) = env.get(k).cloned().or_else(|| std::env::var(k).ok()) {
            unique_push(&mut set, &mut out, PathBuf::from(v));
        }
    }
    // 3) User roots
    if let Some(up) = std::env::var_os("USERPROFILE") {
        unique_push(&mut set, &mut out, PathBuf::from(up));
    }
    if let Some(pubp) = std::env::var_os("PUBLIC") {
        unique_push(&mut set, &mut out, PathBuf::from(pubp));
    }
    // 4) PATH entries (best-effort)
    if let Some(path) = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
    {
        for part in std::env::split_paths(OsStr::new(&path)) {
            if !part.as_os_str().is_empty() {
                unique_push(&mut set, &mut out, part);
            }
        }
    }
    // 5) Core system roots last
    for p in [PathBuf::from("C:/"), PathBuf::from("C:/Windows")] {
        unique_push(&mut set, &mut out, p);
    }
    out
}

unsafe fn path_has_world_write_allow(path: &Path) -> Result<bool> {
    let mut world = world_sid()?;
    let psid_world = world.as_mut_ptr() as *mut c_void;
    let write_mask = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;
    // SAFETY: world owns the successfully constructed SID throughout this check.
    unsafe {
        path_mask_allows(
            path,
            &[psid_world],
            write_mask,
            /*require_all_bits*/ false,
        )
    }
}

#[derive(Default)]
struct WorldWritableScan {
    paths: Vec<PathBuf>,
    incomplete: bool,
}

fn audit_everyone_writable(
    cwd: &Path,
    env: &std::collections::HashMap<String, String>,
    logs_base_dir: Option<&Path>,
) -> Result<WorldWritableScan> {
    audit_everyone_writable_with_timeout(
        cwd,
        env,
        logs_base_dir,
        Duration::from_secs(AUDIT_TIME_LIMIT_SECS as u64),
    )
}

fn audit_everyone_writable_with_timeout(
    cwd: &Path,
    env: &std::collections::HashMap<String, String>,
    logs_base_dir: Option<&Path>,
    time_limit: Duration,
) -> Result<WorldWritableScan> {
    audit_everyone_writable_with_candidates(cwd, logs_base_dir, time_limit, || {
        gather_candidates(cwd, env)
    })
}

fn audit_everyone_writable_with_candidates(
    cwd: &Path,
    logs_base_dir: Option<&Path>,
    time_limit: Duration,
    candidates: impl FnOnce() -> Vec<PathBuf>,
) -> Result<WorldWritableScan> {
    let start = Instant::now();
    let mut incomplete = false;
    let mut flagged: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut checked = 0usize;
    let check_world_writable = |path: &Path, incomplete: &mut bool| -> bool {
        // SAFETY: path is borrowed for the synchronous audit; path_has_world_write_allow creates
        // and retains its SID and security-descriptor storage internally.
        match unsafe { path_has_world_write_allow(path) } {
            Ok(has) => has,
            Err(err) => {
                *incomplete = true;
                debug_log(
                    &format!("AUDIT: unable to inspect ACL: {} ({err})", path.display()),
                    logs_base_dir,
                );
                false
            }
        }
    };
    // Fast path: check CWD immediate children first so workspace issues are caught early.
    let read = std::fs::read_dir(cwd);
    if read.is_err() {
        incomplete = true;
    }
    if let Ok(read) = read {
        for (index, ent) in read.enumerate() {
            if index >= MAX_ITEMS_PER_DIR as usize {
                incomplete = true;
                break;
            }
            if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
                incomplete = true;
                break;
            }
            let ent = match ent {
                Ok(ent) => ent,
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = ent.path();
            checked += 1;
            let has = check_world_writable(&p, &mut incomplete);
            if has {
                let key = canonical_path_key(&p);
                if seen.insert(key) {
                    flagged.push(p);
                }
            }
        }
    }
    // Continue with broader candidate sweep
    for root in candidates() {
        if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
            incomplete = true;
            break;
        }
        checked += 1;
        let has_root = check_world_writable(&root, &mut incomplete);
        if has_root {
            let key = canonical_path_key(&root);
            if seen.insert(key) {
                flagged.push(root.clone());
            }
        }
        // one level down best-effort
        let read = std::fs::read_dir(&root);
        if read.is_err() {
            incomplete = true;
        }
        if let Ok(read) = read {
            for (index, ent) in read.enumerate() {
                if index >= MAX_ITEMS_PER_DIR as usize {
                    incomplete = true;
                    break;
                }
                let ent = match ent {
                    Ok(ent) => ent,
                    Err(_) => {
                        incomplete = true;
                        continue;
                    }
                };
                let p = ent.path();
                if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
                    incomplete = true;
                    break;
                }
                // Skip reparse points (symlinks/junctions) to avoid auditing link ACLs
                let ft = match ent.file_type() {
                    Ok(ft) => ft,
                    Err(_) => {
                        incomplete = true;
                        continue;
                    }
                };
                if ft.is_symlink() {
                    continue;
                }
                // Skip noisy/irrelevant Windows system subdirectories
                let pl = p.to_string_lossy().to_ascii_lowercase();
                let norm = pl.replace('\\', "/");
                if SKIP_DIR_SUFFIXES.iter().any(|s| norm.ends_with(s)) {
                    continue;
                }
                if ft.is_dir() {
                    checked += 1;
                    let has_child = check_world_writable(&p, &mut incomplete);
                    if has_child {
                        let key = canonical_path_key(&p);
                        if seen.insert(key) {
                            flagged.push(p);
                        }
                    }
                }
            }
        }
    }
    let elapsed_ms = start.elapsed().as_millis();
    if incomplete {
        log_note(
            &format!(
                "AUDIT: world-writable scan INCOMPLETE; read failure or time/item limit reached; checked={checked}; duration_ms={elapsed_ms}"
            ),
            logs_base_dir,
        );
    }
    if !flagged.is_empty() {
        let mut list = String::new();
        for p in &flagged {
            let _ = write!(list, "\n - {}", p.display());
        }
        crate::logging::log_note(
            &format!(
                "AUDIT: world-writable scan FAILED; cwd={cwd:?}; checked={checked}; duration_ms={elapsed_ms}; flagged:{list}",
            ),
            logs_base_dir,
        );

        return Ok(WorldWritableScan {
            paths: flagged,
            incomplete,
        });
    }
    if !incomplete {
        log_note(
            &format!("AUDIT: world-writable scan OK; checked={checked}; duration_ms={elapsed_ms}"),
            logs_base_dir,
        );
    }
    Ok(WorldWritableScan {
        paths: flagged,
        incomplete,
    })
}

pub fn apply_world_writable_scan_and_denies_for_permissions(
    codex_home: &Path,
    cwd: &Path,
    env_map: &std::collections::HashMap<String, String>,
    permissions: &ResolvedWindowsSandboxPermissions,
) -> Result<()> {
    // Audit notes belong in the daily sandbox log, not in a second log beside CODEX_HOME's files.
    let logs_base_dir = sandbox_dir(codex_home);
    let logs_base_dir = Some(logs_base_dir.as_path());
    let scan = audit_everyone_writable(cwd, env_map, logs_base_dir)?;
    // Preserve remediation of paths already found even when the scan hit its limit.
    if let Err(err) = apply_capability_denies_for_world_writable_for_permissions(
        codex_home,
        &scan.paths,
        permissions,
        cwd,
        env_map,
        logs_base_dir,
    ) {
        log_note(
            &format!("AUDIT: failed to apply capability deny ACEs: {err}"),
            logs_base_dir,
        );
        return Err(err);
    }
    if scan.incomplete {
        anyhow::bail!("world-writable scan incomplete: read failure or time/item limit reached");
    }
    Ok(())
}

fn apply_capability_denies_for_world_writable_for_permissions(
    codex_home: &Path,
    flagged: &[PathBuf],
    permissions: &ResolvedWindowsSandboxPermissions,
    cwd: &Path,
    env_map: &std::collections::HashMap<String, String>,
    logs_base_dir: Option<&Path>,
) -> Result<()> {
    if flagged.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(codex_home)?;
    let caps = load_or_create_cap_sids(codex_home)?;
    if !permissions.is_enforceable_by_windows_sandbox() {
        return Ok(());
    }
    let (active_sids, workspace_roots): (Vec<LocalSid>, Vec<PathBuf>) =
        if permissions.uses_write_capabilities_for_cwd(cwd, env_map) {
            let roots = effective_write_roots_for_permissions(
                permissions,
                cwd,
                env_map,
                codex_home,
                /*write_roots_override*/ None,
            );
            let active_sids = roots
                .iter()
                .map(|root| {
                    workspace_write_cap_sid_for_root(codex_home, cwd, root)
                        .and_then(|sid| LocalSid::from_string(&sid))
                })
                .collect::<Result<Vec<_>>>()?;
            (active_sids, roots)
        } else {
            (vec![LocalSid::from_string(&caps.readonly)?], Vec::new())
        };
    let mut first_error = None;
    for path in flagged {
        if workspace_roots
            .iter()
            .any(|root| workspace_write_root_contains_path(root, path))
        {
            continue;
        }
        for active_sid in &active_sids {
            // SAFETY: active_sid is a LocalSid retained by active_sids while the synchronous ACL
            // helper borrows its valid pointer.
            let res = unsafe { add_deny_write_ace(path, active_sid.as_ptr()) };
            match res {
                Ok(true) => log_note(
                    &format!("AUDIT: applied capability deny ACE to {}", path.display()),
                    logs_base_dir,
                ),
                Ok(false) => {}
                Err(err) => {
                    log_note(
                        &format!(
                            "AUDIT: failed to apply capability deny ACE to {}: {}",
                            path.display(),
                            err
                        ),
                        logs_base_dir,
                    );
                    // Continue protecting the remaining paths, but never report verified
                    // protection when any required deny could not be installed.
                    first_error.get_or_insert_with(|| {
                        err.context(format!("apply capability deny ACE to {}", path.display()))
                    });
                }
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

