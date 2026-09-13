use anyhow::Result;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::LookupAccountNameW;
use windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE;
use windows_sys::Win32::Security::SID_NAME_USE;
use windows_sys::Win32::System::Diagnostics::Debug::FORMAT_MESSAGE_ALLOCATE_BUFFER;
use windows_sys::Win32::System::Diagnostics::Debug::FORMAT_MESSAGE_FROM_SYSTEM;
use windows_sys::Win32::System::Diagnostics::Debug::FORMAT_MESSAGE_IGNORE_INSERTS;
use windows_sys::Win32::System::Diagnostics::Debug::FormatMessageW;

pub fn to_wide<S: AsRef<OsStr>>(s: S) -> Vec<u16> {
    let mut v: Vec<u16> = s.as_ref().encode_wide().collect();
    v.push(0);
    v
}

pub use codex_shell_command::quote_windows_arg;

/// Build a Windows command line for CreateProcess-style APIs.
pub fn argv_to_command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

// Produce a readable description for a Win32 error code.
pub fn format_last_error(err: i32) -> String {
    // SAFETY: The allocation flag makes buf_ptr an output pointer; the checked result describes
    // initialized UTF-16 data copied before its LocalFree.
    unsafe {
        let mut buf_ptr: *mut u16 = std::ptr::null_mut();
        let flags = FORMAT_MESSAGE_ALLOCATE_BUFFER
            | FORMAT_MESSAGE_FROM_SYSTEM
            | FORMAT_MESSAGE_IGNORE_INSERTS;
        let len = FormatMessageW(
            flags,
            std::ptr::null(),
            err as u32,
            0,
            // FORMAT_MESSAGE_ALLOCATE_BUFFER expects a pointer to receive the allocated buffer.
            // Cast &mut *mut u16 to *mut u16 as required by windows-sys.
            (&mut buf_ptr as *mut *mut u16) as *mut u16,
            0,
            std::ptr::null_mut(),
        );
        if len == 0 || buf_ptr.is_null() {
            return format!("Win32 error {err}");
        }
        let slice = std::slice::from_raw_parts(buf_ptr, len as usize);
        let mut s = String::from_utf16_lossy(slice);
        s = s.trim().to_string();
        let _ = LocalFree(buf_ptr as HLOCAL);
        s
    }
}

pub fn string_from_sid_bytes(sid: &[u8]) -> Result<String, String> {
    // A SID contains an eight-byte header followed by its declared DWORD subauthorities.
    const HEADER_LEN: usize = 8;
    if sid.len() < HEADER_LEN {
        return Err("SID is shorter than its header".to_string());
    }
    let sid_len = HEADER_LEN + usize::from(sid[1]) * std::mem::size_of::<u32>();
    if sid_len > SECURITY_MAX_SID_SIZE as usize || sid.len() < sid_len {
        return Err("SID has an invalid or truncated subauthority array".to_string());
    }
    #[repr(C, align(4))]
    struct AlignedSid([u8; SECURITY_MAX_SID_SIZE as usize]);
    let mut aligned_sid = AlignedSid([0; SECURITY_MAX_SID_SIZE as usize]);
    aligned_sid.0[..sid_len].copy_from_slice(&sid[..sid_len]);

    // SAFETY: aligned_sid contains the complete declared SID in DWORD-aligned storage.
    // The successful call returns a NUL-terminated UTF-16 allocation, which remains
    // live through copying and is released once with LocalFree.
    unsafe {
        let mut str_ptr: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(aligned_sid.0.as_mut_ptr().cast(), &mut str_ptr);
        if ok == 0 || str_ptr.is_null() {
            return Err(format!(
                "ConvertSidToStringSidW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut len = 0;
        while *str_ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(str_ptr, len);
        let out = String::from_utf16_lossy(slice);
        let _ = LocalFree(str_ptr as HLOCAL);
        Ok(out)
    }
}

const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
pub const WELL_KNOWN_USERS_SID: &str = "S-1-5-32-545";
const SID_AUTHENTICATED_USERS: &str = "S-1-5-11";
const SID_EVERYONE: &str = "S-1-1-0";
const SID_SYSTEM: &str = "S-1-5-18";

pub fn resolve_sid(name: &str) -> Result<Vec<u8>> {
    if let Some(sid_str) = well_known_sid_str(name) {
        return sid_bytes_from_string(sid_str);
    }
    let name_w = to_wide(OsStr::new(name));
    let mut sid_buffer = vec![0u8; 68];
    let mut sid_len: u32 = sid_buffer.len() as u32;
    let mut domain: Vec<u16> = Vec::new();
    let mut domain_len: u32 = 0;
    let mut use_type: SID_NAME_USE = 0;
    loop {
        // SAFETY: The SID and domain output capacities match the lengths passed in; all output
        // counters are writable, and name_w is terminated and live.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                name_w.as_ptr(),
                sid_buffer.as_mut_ptr() as *mut std::ffi::c_void,
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut use_type,
            )
        };
        if ok != 0 {
            sid_buffer.truncate(sid_len as usize);
            return Ok(sid_buffer);
        }
        // SAFETY: GetLastError reads this thread's error immediately after LookupAccountNameW
        // failed.
        let err = unsafe { GetLastError() };
        if err == ERROR_INSUFFICIENT_BUFFER {
            sid_buffer.resize(sid_len as usize, 0);
            domain.resize(domain_len as usize, 0);
            continue;
        }
        return Err(anyhow::anyhow!(
            "LookupAccountNameW failed for {name}: {err}"
        ));
    }
}

fn well_known_sid_str(name: &str) -> Option<&'static str> {
    match name {
        "Administrators" => Some(SID_ADMINISTRATORS),
        "Users" => Some(WELL_KNOWN_USERS_SID),
        "Authenticated Users" => Some(SID_AUTHENTICATED_USERS),
        "Everyone" => Some(SID_EVERYONE),
        "SYSTEM" => Some(SID_SYSTEM),
        _ => None,
    }
}

fn sid_bytes_from_string(sid_str: &str) -> Result<Vec<u8>> {
    let sid_w = to_wide(OsStr::new(sid_str));
    let mut psid: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: sid_w is a live terminated UTF-16 string and psid is writable pointer storage for
    // the newly allocated SID.
    if unsafe { ConvertStringSidToSidW(sid_w.as_ptr(), &mut psid) } == 0 {
        return Err(anyhow::anyhow!(
            "ConvertStringSidToSidW failed for {sid_str}: {}",
            unsafe { GetLastError() }
        ));
    }
    // SAFETY: psid is a valid SID returned by successful ConvertStringSidToSidW and has not yet
    // been freed.
    let sid_len = unsafe { GetLengthSid(psid) };
    if sid_len == 0 {
        // SAFETY: psid is the uniquely owned allocation returned by ConvertStringSidToSidW and
        // is freed once on this error path.
        unsafe {
            LocalFree(psid as _);
        }
        return Err(anyhow::anyhow!("GetLengthSid failed for {sid_str}"));
    }
    let mut out = vec![0u8; sid_len as usize];
    // SAFETY: psid remains a live converted SID; out is initialized writable storage of exactly
    // the SID length passed to CopySid.
    let ok = unsafe { CopySid(sid_len, out.as_mut_ptr() as *mut std::ffi::c_void, psid) };
    // SAFETY: psid is the uniquely owned conversion result; CopySid has finished reading it
    // before this single free.
    unsafe {
        LocalFree(psid as _);
    }
    if ok == 0 {
        return Err(anyhow::anyhow!("CopySid failed for {sid_str}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::argv_to_command_line;
    use super::resolve_sid;
    use super::string_from_sid_bytes;
    use pretty_assertions::assert_eq;

    #[test]
    fn argv_to_command_line_quotes_each_argument_independently() {
        let argv = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\" -NoProfile -EncodedCommand abc=="
                .to_string(),
        ];

        assert_eq!(
            argv_to_command_line(&argv),
            "cmd.exe /c \"\\\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\\\" -NoProfile -EncodedCommand abc==\""
        );
    }

    #[test]
    fn argv_to_command_line_quotes_regular_program_args() {
        let argv = vec![
            "pwsh.exe".to_string(),
            "-Command".to_string(),
            "Write-Output \"hello world\"".to_string(),
        ];

        assert_eq!(
            argv_to_command_line(&argv),
            "pwsh.exe -Command \"Write-Output \\\"hello world\\\"\""
        );
    }

    #[test]
    fn well_known_accounts_resolve_through_the_canonical_sid_table() {
        let cases = [
            ("Administrators", "S-1-5-32-544"),
            ("Users", "S-1-5-32-545"),
            ("Authenticated Users", "S-1-5-11"),
            ("Everyone", "S-1-1-0"),
            ("SYSTEM", "S-1-5-18"),
        ];

        for (account, expected_sid) in cases {
            let sid = resolve_sid(account).expect("resolve well-known SID");
            assert_eq!(
                string_from_sid_bytes(&sid).expect("format well-known SID"),
                expected_sid
            );
        }
    }

    #[test]
    fn public_sid_conversion_checks_lengths_and_accepts_unaligned_bytes() {
        let administrators = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0];
        assert_eq!(
            crate::string_from_sid_bytes(&administrators).expect("format administrators SID"),
            "S-1-5-32-544"
        );
        for end in 0..administrators.len() {
            assert!(
                crate::string_from_sid_bytes(&administrators[..end]).is_err(),
                "truncated SID of length {end} must fail before Win32 reads beyond the slice"
            );
        }

        #[repr(C, align(4))]
        struct UnalignedSid([u8; 17]);
        let mut unaligned = UnalignedSid([0; 17]);
        unaligned.0[1..].copy_from_slice(&administrators);
        assert_eq!(
            crate::string_from_sid_bytes(&unaligned.0[1..]).expect("format unaligned SID bytes"),
            "S-1-5-32-544"
        );

        let mut invalid_count = [0; 68];
        invalid_count[..administrators.len()].copy_from_slice(&administrators);
        invalid_count[1] = 16;
        assert!(crate::string_from_sid_bytes(&invalid_count).is_err());
    }

    #[test]
    fn current_user_resolves_through_the_canonical_lookup_fallback() {
        let username = std::env::var("USERNAME").expect("USERNAME should identify current user");
        let domain =
            std::env::var("USERDOMAIN").expect("USERDOMAIN should identify current domain");
        let account = format!(r"{domain}\{username}");

        let sid = resolve_sid(&account).expect("resolve current user SID");
        let identity = std::process::Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("query the process identity independently");
        assert!(identity.status.success());
        let identity = String::from_utf8_lossy(&identity.stdout);
        let expected_sid = identity
            .trim()
            .rsplit(',')
            .next()
            .expect("whoami emits a SID column")
            .trim_matches('"');
        assert_eq!(
            string_from_sid_bytes(&sid).expect("format current user SID"),
            expected_sid
        );
    }
}
