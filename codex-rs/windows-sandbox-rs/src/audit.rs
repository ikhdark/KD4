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
    let start = Instant::now();
    let mut incomplete = false;
    let mut flagged: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut checked = 0usize;
    let check_world_writable = |path: &Path| -> bool {
        // SAFETY: path is borrowed for the synchronous audit; path_has_world_write_allow creates
        // and retains its SID and security-descriptor storage internally.
        match unsafe { path_has_world_write_allow(path) } {
            Ok(has) => has,
            Err(err) => {
                debug_log(
                    &format!(
                        "AUDIT: treating unreadable ACL as not world-writable: {} ({err})",
                        path.display()
                    ),
                    logs_base_dir,
                );
                false
            }
        }
    };
    // Fast path: check CWD immediate children first so workspace issues are caught early.
    if let Ok(read) = std::fs::read_dir(cwd) {
        for ent in read.flatten().take(MAX_ITEMS_PER_DIR as usize) {
            if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
                incomplete = true;
                break;
            }
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = ent.path();
            checked += 1;
            let has = check_world_writable(&p);
            if has {
                let key = canonical_path_key(&p);
                if seen.insert(key) {
                    flagged.push(p);
                }
            }
        }
    }
    // Continue with broader candidate sweep
    let candidates = gather_candidates(cwd, env);
    for root in candidates {
        if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
            incomplete = true;
            break;
        }
        checked += 1;
        let has_root = check_world_writable(&root);
        if has_root {
            let key = canonical_path_key(&root);
            if seen.insert(key) {
                flagged.push(root.clone());
            }
        }
        // one level down best-effort
        if let Ok(read) = std::fs::read_dir(&root) {
            for ent in read.flatten().take(MAX_ITEMS_PER_DIR as usize) {
                let p = ent.path();
                if start.elapsed() >= time_limit || checked >= MAX_CHECKED_LIMIT as usize {
                    incomplete = true;
                    break;
                }
                // Skip reparse points (symlinks/junctions) to avoid auditing link ACLs
                let ft = match ent.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
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
                    let has_child = check_world_writable(&p);
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
                "AUDIT: world-writable scan INCOMPLETE; time or item limit reached; checked={checked}; duration_ms={elapsed_ms}"
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
    }
    if scan.incomplete {
        anyhow::bail!("world-writable scan incomplete: time or item limit reached");
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
                Err(err) => log_note(
                    &format!(
                        "AUDIT: failed to apply capability deny ACE to {}: {}",
                        path.display(),
                        err
                    ),
                    logs_base_dir,
                ),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::WorldWritableScan;
    use super::gather_candidates;
    use super::world_writable_warning_details_from_scan;
    use anyhow::anyhow;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn applying_audit_denies_reuses_persisted_capabilities_without_rewriting() -> anyhow::Result<()>
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::HLOCAL;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let home = tempfile::tempdir()?;
        let flagged = tempfile::tempdir()?;
        let caps = crate::cap::load_or_create_cap_sids(home.path())?;
        let sid = crate::token::LocalSid::from_string(&caps.readonly)?;
        let has_deny = || -> anyhow::Result<bool> {
            // SAFETY: the temporary directory exists and LocalSid owns a valid SID.
            // The descriptor is released after the borrowed DACL has been inspected.
            unsafe {
                let (dacl, descriptor) = crate::acl::fetch_dacl_handle(flagged.path())?;
                let present = crate::acl::dacl_has_write_deny_for_sid(dacl, sid.as_ptr());
                LocalFree(descriptor as HLOCAL);
                Ok(present)
            }
        };
        assert!(!has_deny()?);
        let cap_path = crate::cap::cap_sid_file(home.path());
        let before = fs::read(&cap_path)?;
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&cap_path)?;
        let permissions = crate::resolved_permissions::ResolvedWindowsSandboxPermissions::
            try_from_permission_profile(&codex_protocol::models::PermissionProfile::read_only())?;

        super::apply_capability_denies_for_world_writable_for_permissions(
            home.path(),
            &[flagged.path().to_path_buf()],
            &permissions,
            flagged.path(),
            &HashMap::new(),
            None,
        )?;

        assert!(
            has_deny()?,
            "the flagged path must receive its capability deny"
        );
        assert_eq!(fs::read(&cap_path)?, before);
        Ok(())
    }

    #[test]
    fn gathers_path_entries_by_list_separator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir_a = tmp.path().join("Tools");
        let dir_b = tmp.path().join("Bin");
        let dir_space = tmp.path().join("Program Files");
        fs::create_dir_all(&dir_a).expect("dir a");
        fs::create_dir_all(&dir_b).expect("dir b");
        fs::create_dir_all(&dir_space).expect("dir space");

        let mut env_map = HashMap::new();
        env_map.insert(
            "PATH".to_string(),
            format!(
                "{};{};{}",
                dir_a.display(),
                dir_b.display(),
                dir_space.display()
            ),
        );

        let candidates = gather_candidates(tmp.path(), &env_map);
        let canon_a = dir_a.canonicalize().expect("canon a");
        let canon_b = dir_b.canonicalize().expect("canon b");
        let canon_space = dir_space.canonicalize().expect("canon space");

        assert!(candidates.contains(&canon_a));
        assert!(candidates.contains(&canon_b));
        assert!(candidates.contains(&canon_space));
    }

    #[test]
    fn warning_details_sample_paths_and_report_scan_failures() {
        let details = world_writable_warning_details_from_scan(Ok(WorldWritableScan {
            paths: vec![
                "C:/one".into(),
                "C:/two".into(),
                "C:/three".into(),
                "C:/four".into(),
            ],
            incomplete: false,
        }))
        .expect("non-empty scan should produce warning details");
        assert_eq!(
            details,
            (
                vec![
                    "C:\\one".to_string(),
                    "C:\\two".to_string(),
                    "C:\\three".to_string(),
                ],
                1,
                false,
            )
        );

        assert_eq!(
            world_writable_warning_details_from_scan(Ok(WorldWritableScan::default())),
            None
        );
        assert_eq!(
            world_writable_warning_details_from_scan(Err(anyhow!("scan failed"))),
            Some((Vec::new(), 0, true))
        );
    }

    #[test]
    fn timed_out_audit_reports_incomplete_instead_of_success() -> anyhow::Result<()> {
        let cwd = tempfile::tempdir()?;
        let logs = tempfile::tempdir()?;
        let scan = super::audit_everyone_writable_with_timeout(
            cwd.path(),
            &HashMap::new(),
            Some(logs.path()),
            std::time::Duration::ZERO,
        )?;
        assert!(scan.incomplete);
        assert!(scan.paths.is_empty());
        assert_eq!(
            world_writable_warning_details_from_scan(Ok(scan)),
            Some((Vec::new(), 0, true))
        );
        let log = fs::read_to_string(crate::logging::current_log_file_path(logs.path()))?;
        assert!(log.contains("world-writable scan INCOMPLETE"));
        assert!(!log.contains("world-writable scan OK"));
        Ok(())
    }

    #[test]
    fn warning_scan_logs_to_the_sandbox_log_instead_of_codex_home() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let cwd = tempfile::tempdir()?;
        fs::create_dir_all(crate::setup::sandbox_dir(home.path()))?;

        let _ = super::world_writable_warning_details(home.path(), cwd.path());

        let sandbox_log = fs::read_to_string(
            crate::logging::current_log_file_path_for_codex_home(home.path()),
        )?;
        assert!(sandbox_log.contains("AUDIT: world-writable scan"));
        assert!(!crate::logging::current_log_file_path(home.path()).exists());
        Ok(())
    }

    #[test]
    fn incomplete_audit_preserves_flagged_paths_in_warning() {
        let scan = WorldWritableScan {
            paths: vec!["C:/flagged".into()],
            incomplete: true,
        };
        assert_eq!(
            world_writable_warning_details_from_scan(Ok(scan)),
            Some((vec!["C:\\flagged".to_string()], 0, true))
        );
    }
}
