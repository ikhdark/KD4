use super::SetupPayload;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::windows::io::FromRawHandle;
use tempfile::NamedTempFile;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

pub const SETUP_PAYLOAD_FILE_ARG: &str = "--payload-file";
pub const SETUP_PAYLOAD_STDIN_ARG: &str = "--payload-stdin";
const MAX_SETUP_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// The caller retains the temporary file through helper exit, or transfers its
/// open handle as stdin. Bulk policy data never needs to fit in Windows argv.
pub fn write_setup_payload(payload: &SetupPayload) -> Result<NamedTempFile> {
    let bytes = serde_json::to_vec(payload).context("serialize setup payload")?;
    if bytes.len() as u64 > MAX_SETUP_PAYLOAD_BYTES {
        bail!("setup payload exceeds {MAX_SETUP_PAYLOAD_BYTES} bytes");
    }
    let mut file = tempfile::Builder::new()
        .prefix("codex-sandbox-payload-")
        .make(create_payload_file)
        .context("create protected setup payload file")?;
    file.write_all(&bytes).context("write setup payload")?;
    file.rewind().context("rewind setup payload")?;
    Ok(file)
}

fn create_payload_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    // Apply the protected DACL at creation, before any policy data is written.
    // Owner rights preserve the creator's access; administrators can read after UAC.
    let sddl = crate::to_wide("D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: sddl is terminated; descriptor is writable and retained through
    // CreateFileW, which copies it before LocalFree below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let path = crate::to_wide(path);
    // SAFETY: all pointers remain valid for the synchronous call. CREATE_NEW
    // refuses existing paths; only a successful owned handle is wrapped below.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_TEMPORARY,
            std::ptr::null_mut(),
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: descriptor is the successful conversion allocation, no longer borrowed.
    unsafe {
        LocalFree(descriptor);
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: handle is a valid, uniquely owned CreateFileW result.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

pub fn read_setup_payload(args: &[String], stdin: impl Read) -> Result<SetupPayload> {
    let bytes = match args {
        [flag] if flag == SETUP_PAYLOAD_STDIN_ARG => read_bounded_payload(stdin)?,
        [flag, path] if flag == SETUP_PAYLOAD_FILE_ARG => {
            read_bounded_payload(std::fs::File::open(path).context("open setup payload file")?)?
        }
        [legacy] => {
            // Older launchers remain accepted; new launchers never send large argv.
            if legacy.len() as u64 > MAX_SETUP_PAYLOAD_BYTES {
                bail!("encoded setup payload exceeds {MAX_SETUP_PAYLOAD_BYTES} bytes");
            }
            base64::engine::general_purpose::STANDARD
                .decode(legacy)
                .context("decode legacy setup payload")?
        }
        _ => bail!("expected --payload-stdin, --payload-file PATH, or legacy payload"),
    };
    serde_json::from_slice(&bytes).context("parse setup payload JSON")
}

fn read_bounded_payload(input: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(MAX_SETUP_PAYLOAD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SETUP_PAYLOAD_BYTES {
        bail!("setup payload exceeds {MAX_SETUP_PAYLOAD_BYTES} bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    fn payload() -> SetupPayload {
        serde_json::from_value(serde_json::json!({
            "version": crate::SETUP_VERSION, "offline_username": "offline", "online_username": "online",
            "codex_home": "C:\\fixture", "command_cwd": "C:\\fixture", "read_roots": [], "write_roots": [],
            "deny_read_paths": (0..1000).map(|i| format!("C:\\fixture\\packages\\project-{i:04}\\production.env")).collect::<Vec<_>>(),
            "proxy_ports": [], "real_user": "fixture"
        })).unwrap()
    }

    #[test]
    fn large_payload_reaches_a_real_child_and_is_removed_after_use() {
        let expected = payload();
        let file = write_setup_payload(&expected).unwrap();
        assert_private_dacl(file.as_file());
        let path = file.path().to_path_buf();
        assert!(file.as_file().metadata().unwrap().len() > 32767);
        for from_stdin in [false, true] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "setup_protocol::transport::tests::payload_child",
                    "--nocapture",
                ])
                .env(
                    "CODEX_TEST_SETUP_PAYLOAD",
                    if from_stdin {
                        "stdin".into()
                    } else {
                        path.as_os_str().to_os_string()
                    },
                )
                .creation_flags(0x08000000);
            if from_stdin {
                command.stdin(file.reopen().unwrap());
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("verified 1000 deny paths"));
        }
        let legacy = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&expected).unwrap());
        assert_eq!(
            serde_json::to_value(read_setup_payload(&[legacy], std::io::empty()).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        drop(file);
        assert!(!path.exists());

        // Background helpers own only stdin, not the parent's temporary path.
        let file = write_setup_payload(&payload()).unwrap();
        let path = file.path().to_path_buf();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "setup_protocol::transport::tests::payload_child",
                "--nocapture",
            ])
            .env("CODEX_TEST_SETUP_PAYLOAD", "stdin")
            .stdin(file.into_file())
            .creation_flags(0x08000000);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("verified 1000 deny paths"));
        drop(command);
        assert!(!path.exists());
    }

    fn assert_private_dacl(file: &std::fs::File) {
        use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
        use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
        use windows_sys::Win32::Security::*;
        let mut descriptor = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        // SAFETY: the file handle is live; returned buffers belong to descriptor
        // until LocalFree. Every ACE is read only within this returned ACL.
        unsafe {
            assert_eq!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut acl,
                    std::ptr::null_mut(),
                    &mut descriptor
                ),
                0
            );
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 3);
            let mut identities = Vec::new();
            for index in 0..3 {
                let mut ace = std::ptr::null_mut();
                assert_ne!(GetAce(acl, index, &mut ace), 0);
                let ace = &*(ace as *const ACCESS_ALLOWED_ACE);
                assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
                let sid = (&raw const ace.SidStart).cast_mut().cast();
                let kind = [
                    WinCreatorOwnerRightsSid,
                    WinLocalSystemSid,
                    WinBuiltinAdministratorsSid,
                ]
                .into_iter()
                .find(|kind| IsWellKnownSid(sid, *kind) != 0);
                identities.push(kind.expect("unexpected identity can access policy data"));
            }
            identities.sort();
            identities.dedup();
            assert_eq!(identities.len(), 3);
            LocalFree(descriptor);
        }
    }

    #[test]
    fn payload_child() {
        let Some(mode) = std::env::var_os("CODEX_TEST_SETUP_PAYLOAD") else {
            return;
        };
        let args = if mode == "stdin" {
            vec![SETUP_PAYLOAD_STDIN_ARG.into()]
        } else {
            vec![SETUP_PAYLOAD_FILE_ARG.into(), mode.into_string().unwrap()]
        };
        let payload = read_setup_payload(&args, std::io::stdin().lock()).unwrap();
        assert_eq!(payload.deny_read_paths.len(), 1000);
        for (i, path) in payload.deny_read_paths.iter().enumerate() {
            assert_eq!(
                path,
                &std::path::PathBuf::from(format!(
                    "C:\\fixture\\packages\\project-{i:04}\\production.env"
                ))
            );
        }
        println!("verified 1000 deny paths");
    }

    #[test]
    fn invalid_and_oversized_payloads_are_rejected() {
        assert!(read_setup_payload(&[], std::io::empty()).is_err());
        assert!(
            read_setup_payload(&[SETUP_PAYLOAD_STDIN_ARG.into()], b"not JSON".as_slice()).is_err()
        );
        assert!(
            read_bounded_payload(std::io::repeat(b' ').take(MAX_SETUP_PAYLOAD_BYTES + 1)).is_err()
        );
    }
}
