use std::collections::HashMap;
use std::path::PathBuf;
use crate::setup_protocol::{PreparedSetupRequest, SetupMode, SetupPayload, read_setup_request};

fn payload(home: &std::path::Path) -> SetupPayload {
    SetupPayload {
        version: crate::setup::SETUP_VERSION, offline_username: "offline".into(), online_username: "online".into(),
        codex_home: home.into(), command_cwd: home.into(), read_roots: Vec::new(), write_roots: Vec::new(),
        deny_read_paths: Vec::new(), deny_write_paths: Vec::new(), proxy_ports: Vec::new(), allow_local_binding: false,
        otel: None, real_user: "fixture".into(), mode: SetupMode::Full, refresh_only: true,
    }
}

#[test]
fn audit20261023_corrupt_capability_state_never_rotates_persisted_identities() -> anyhow::Result<()> {
    for bytes in [b"".as_slice(), b"{broken", b"not-a-sid", b"S-1-1-0", br#"{"workspace":"invalid","readonly":"S-1-5-21-1-2-3-4"}"#, br#"{"readonly":"S-1-5-21-1-2-3-4"}"#] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("cap_sid");
        std::fs::write(&path, bytes)?;
        assert!(crate::cap::load_or_create_cap_sids(home.path()).is_err(), "{bytes:?}");
        assert_eq!(std::fs::read(path)?, bytes);
    }
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("cap_sid"), "S-1-5-21-1-2-3-4")?;
    let caps = crate::cap::load_or_create_cap_sids(home.path())?;
    assert_eq!(caps.workspace, "S-1-5-21-1-2-3-4");
    assert_ne!(caps.readonly, caps.workspace);
    Ok(())
}

#[test]
fn audit20261023_grant_identities_are_lossless_and_do_not_reuse_legacy_keys() -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    let home = tempfile::tempdir()?;
    let upper = home.path().join("MissingRoot");
    let lower = home.path().join("missingroot");
    assert_ne!(crate::path_normalization::security_path_key(&upper), crate::path_normalization::security_path_key(&lower));
    let a = PathBuf::from(std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]));
    let b = PathBuf::from(std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd801]));
    assert_ne!(crate::path_normalization::security_path_key(&a), crate::path_normalization::security_path_key(&b));
    assert_ne!(crate::cap::writable_root_cap_sid_for_path(home.path(), &upper)?, crate::cap::writable_root_cap_sid_for_path(home.path(), &lower)?);
    assert_eq!(crate::cap::writable_root_cap_sid_for_path(home.path(), &upper)?, crate::cap::writable_root_cap_sid_for_path(home.path(), &upper)?);
    Ok(())
}

#[test]
fn audit20261023_windows_environment_preserves_caller_case_aliases_and_rejects_ambiguity() -> anyhow::Result<()> {
    let mut env = HashMap::from([("Path".into(), "caller-path".into()), ("Pathext".into(), ".EXE;.CMD".into()), ("gIt_AlLoW_pRoToCoL".into(), "file".into())]);
    crate::env::normalize_environment(&mut env)?;
    crate::env::inherit_path_env(&mut env);
    assert_eq!(env["PATH"], "caller-path");
    assert_eq!(env["PATHEXT"], ".EXE;.CMD");
    crate::env::apply_no_network_to_env(&mut env)?;
    assert_eq!(env["GIT_ALLOW_PROTOCOL"], "");
    assert_eq!(env.keys().filter(|key| key.eq_ignore_ascii_case("GIT_ALLOW_PROTOCOL")).count(), 1);
    let mut ambiguous = HashMap::from([("Path".into(), "one".into()), ("PATH".into(), "two".into())]);
    assert!(crate::env::normalize_environment(&mut ambiguous).is_err());
    assert_eq!(ambiguous.len(), 2, "failure must not partially rewrite caller state");
    Ok(())
}

#[test]
fn audit20261023_environment_blocks_validate_records_and_empty_termination() -> anyhow::Result<()> {
    assert_eq!(crate::process::make_env_block(&HashMap::new())?, vec![0, 0]);
    for (name, value) in [("", "value"), ("A=B", "value"), ("A\0B", "value"), ("A", "x\0INJECTED=y")] {
        assert!(crate::process::make_env_block(&HashMap::from([(name.into(), value.into())])).is_err());
    }
    let block = crate::process::make_env_block(&HashMap::from([("A".into(), "x=y".into()), ("=c:".into(), "C:\\fixture".into())]))?;
    assert!(block.ends_with(&[0, 0]));
    let text = String::from_utf16(&block)?;
    assert!(text.contains("A=x=y\0"));
    assert!(text.contains("=C:=C:\\fixture\0"));
    Ok(())
}

#[test]
fn audit20261023_large_setup_policy_uses_small_integrity_bound_arguments() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut original = payload(home.path());
    original.deny_read_paths = (0..600).map(|index| home.path().join(format!("denied-{index}-{}", "long-component-".repeat(8)))).collect();
    let staged = PreparedSetupRequest::new(&original)?;
    assert!(staged.payload_len > 32767);
    let args = staged.arguments(false)?;
    assert!(args.iter().map(String::len).sum::<usize>() < 2048);
    let decoded = read_setup_request(&args)?;
    assert_eq!(decoded.deny_read_paths, original.deny_read_paths);
    assert_eq!(decoded.version, original.version);
    assert_eq!(decoded.refresh_only, original.refresh_only);
    let path = PathBuf::from(&args[1]);
    assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err(), "foreground request must be pinned against modification");
    let mut bad_digest = args.clone(); bad_digest[2] = "0".repeat(64);
    assert!(read_setup_request(&bad_digest).is_err());
    let mut bad_length = args.clone(); bad_length[3] = "1".into();
    assert!(read_setup_request(&bad_length).is_err());
    drop(staged);
    assert!(!path.exists());
    Ok(())
}

#[test]
fn audit20261023_background_setup_requests_own_independent_lifetimes() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let original = payload(home.path());
    let foreground = PreparedSetupRequest::new(&original)?;
    let foreground_path = PathBuf::from(&foreground.arguments(false)?[1]);
    let (background_path, args) = PreparedSetupRequest::new(&original)?.into_background()?;
    assert_ne!(foreground_path, background_path);
    assert_eq!(read_setup_request(&args)?.codex_home, original.codex_home);
    assert!(!background_path.exists());
    assert!(foreground_path.exists());
    let (truncated_path, args) = PreparedSetupRequest::new(&original)?.into_background()?;
    std::fs::write(&truncated_path, b"{")?;
    assert!(read_setup_request(&args).is_err());
    std::fs::remove_file(truncated_path)?;
    drop(foreground);
    assert!(!foreground_path.exists());
    Ok(())
}

#[test]
fn audit20261023_protected_ancestor_write_roots_fail_before_acl_admission() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let private_home = parent.path().join("private-home");
    let sibling = parent.path().join("workspace");
    std::fs::create_dir_all(&private_home)?;
    std::fs::create_dir_all(&sibling)?;
    assert!(crate::setup::ensure_safe_write_roots([&parent.path().to_path_buf()], &private_home).is_err());
    crate::setup::ensure_safe_write_roots([&sibling], &private_home)?;
    Ok(())
}
