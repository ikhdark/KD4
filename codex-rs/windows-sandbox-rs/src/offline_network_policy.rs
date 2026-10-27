//! Machine-wide offline-account policy custody. Setup and launch both hold the
//! existing setup mutex. The named outer job (without kill-on-close or breakaway)
//! remains alive through all runner descendants, including intentionally
//! surviving commands. Policy mutation is refused while that job has members.
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
use windows_sys::Win32::System::Registry::HKEY;
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::Registry::KEY_READ;
use windows_sys::Win32::System::Registry::KEY_WOW64_64KEY;
use windows_sys::Win32::System::Registry::KEY_WRITE;
use windows_sys::Win32::System::Registry::REG_OPTION_NON_VOLATILE;
use windows_sys::Win32::System::Registry::REG_SZ;
use windows_sys::Win32::System::Registry::RegCloseKey;
use windows_sys::Win32::System::Registry::RegCreateKeyExW;
use windows_sys::Win32::System::Registry::RegFlushKey;
use windows_sys::Win32::System::Registry::RegOpenKeyExW;
use windows_sys::Win32::System::Registry::RegQueryValueExW;
use windows_sys::Win32::System::Registry::RegSetValueExW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;

const POLICY_JOB: &str = "Global\\CodexSandboxOfflineNetworkPolicy-v1";
const POLICY_KEY: &str = "SOFTWARE\\OpenAI\\CodexSandbox";
const POLICY_VALUE: &str = "OfflineNetworkPolicy-v1";

fn policy_value(username: &str, ports: &[u16], allow_local_binding: bool) -> String {
    let mut ports = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    format!(
        "{}|{ports:?}|{allow_local_binding}",
        username.to_ascii_lowercase()
    )
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: This wrapper uniquely owns the registry handle.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn read_policy() -> Result<Option<String>> {
    let mut key = std::ptr::null_mut();
    // SAFETY: Both paths are terminated buffers, and key is an output slot.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            to_wide(POLICY_KEY).as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    anyhow::ensure!(
        status == 0,
        "open offline network policy registry failed: {status}"
    );
    let key = RegistryKey(key);
    let mut kind = 0;
    let mut bytes = 0;
    let name = to_wide(POLICY_VALUE);
    // SAFETY: This size query borrows the owned key and writes scalar outputs.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    anyhow::ensure!(
        status == 0 && kind == REG_SZ && bytes <= 1024 * 1024 && bytes % 2 == 0,
        "invalid offline network policy registry value: {status}"
    );
    let mut value = vec![0u16; bytes as usize / 2];
    // SAFETY: value has exactly the byte capacity supplied to the registry API.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    anyhow::ensure!(
        status == 0 && kind == REG_SZ && bytes as usize <= value.len() * 2,
        "read offline network policy registry failed: {status}"
    );
    value.truncate(bytes as usize / 2);
    if value.last() == Some(&0) {
        value.pop();
    }
    Ok(Some(
        String::from_utf16(&value).context("offline network policy encoding")?,
    ))
}

fn write_policy(value: &str) -> Result<()> {
    let mut key = std::ptr::null_mut();
    // HKLM's inherited ACL permits setup/admin writes, not sandbox-account writes.
    // SAFETY: Path and output storage live through this synchronous call.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            to_wide(POLICY_KEY).as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_WOW64_64KEY,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(
        status == 0,
        "create offline network policy registry failed: {status}"
    );
    let key = RegistryKey(key);
    let value = to_wide(value);
    // SAFETY: Both UTF-16 buffers are terminated and the data length is exact.
    let status = unsafe {
        RegSetValueExW(
            key.0,
            to_wide(POLICY_VALUE).as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            std::mem::size_of_val(value.as_slice()) as u32,
        )
    };
    anyhow::ensure!(
        status == 0,
        "write offline network policy registry failed: {status}"
    );
    // The invalid marker must survive a crash before any persistent filter
    // replacement; otherwise an old marker could falsely authorize a launch.
    let status = unsafe { RegFlushKey(key.0) };
    anyhow::ensure!(
        status == 0,
        "flush offline network policy registry failed: {status}"
    );
    Ok(())
}

pub(crate) fn policy_matches(
    username: &str,
    ports: &[u16],
    allow_local_binding: bool,
) -> Result<bool> {
    Ok(read_policy()?.as_deref()
        == Some(policy_value(username, ports, allow_local_binding).as_str()))
}

pub(crate) struct OfflinePolicyJob(HANDLE);

impl Drop for OfflinePolicyJob {
    fn drop(&mut self) {
        // SAFETY: This owner holds the job handle. Closing it does not kill members;
        // Windows retains the job itself until its last process also terminates.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl OfflinePolicyJob {
    pub(crate) fn open() -> Result<Self> {
        // Default limits prohibit breakaway and impose no kill-on-close policy.
        // SAFETY: The job name is terminated; null security uses the caller ACL.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), to_wide(POLICY_JOB).as_ptr()) };
        anyhow::ensure!(
            !handle.is_null(),
            "open offline policy lifetime job: {}",
            std::io::Error::last_os_error()
        );
        let job = Self(handle);
        // Reject a sandbox/foreign-account object squatting on the fixed name.
        // A legitimate object's default owner is this host user or an elevated
        // administrator/SYSTEM; its default DACL does not grant sandbox control.
        let mut token = std::ptr::null_mut();
        // SAFETY: Only query access is requested on this host's own process token.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        anyhow::ensure!(
            opened != 0,
            "query offline policy job owner token: {}",
            std::io::Error::last_os_error()
        );
        let user = unsafe { crate::token::get_user_sid_bytes(token) };
        unsafe {
            CloseHandle(token);
        }
        let user = user?;
        let admins = crate::token::LocalSid::from_string("S-1-5-32-544")?;
        let system = crate::token::LocalSid::from_string("S-1-5-18")?;
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: Both outputs are retained until after the SID comparisons.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        let trusted = status == 0
            && !owner.is_null()
            && unsafe {
                EqualSid(owner, user.as_ptr() as _) != 0
                    || EqualSid(owner, admins.as_ptr()) != 0
                    || EqualSid(owner, system.as_ptr()) != 0
            };
        if !descriptor.is_null() {
            unsafe {
                LocalFree(descriptor);
            }
        }
        anyhow::ensure!(
            trusted,
            "offline policy lifetime job has an untrusted owner ({status})"
        );
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let result = unsafe {
            QueryInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of_mut!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
                std::ptr::null_mut(),
            )
        };
        anyhow::ensure!(
            result != 0 && limits.BasicLimitInformation.LimitFlags == 0,
            "offline policy lifetime job must prohibit breakaway without kill-on-close"
        );
        Ok(job)
    }

    fn active_processes(&self) -> Result<u32> {
        // SAFETY: The initialized output covers the exact native structure size.
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let result = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                std::ptr::addr_of_mut!(accounting).cast(),
                std::mem::size_of_val(&accounting) as u32,
                std::ptr::null_mut(),
            )
        };
        anyhow::ensure!(
            result != 0,
            "query offline policy lifetime job: {}",
            std::io::Error::last_os_error()
        );
        Ok(accounting.ActiveProcesses)
    }

    pub(crate) fn attach(&self, process: HANDLE) -> Result<()> {
        // SAFETY: Both handles remain owned by the launch transaction. The root
        // is suspended, so it cannot create untracked descendants before assignment.
        let result = unsafe { AssignProcessToJobObject(self.0, process) };
        anyhow::ensure!(
            result != 0,
            "attach offline policy lifetime job: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

/// Used only by elevated setup while holding the machine-wide setup mutex.
pub struct OfflineNetworkPolicyUpdate {
    _job: OfflinePolicyJob,
    desired: String,
    needed: bool,
}

impl OfflineNetworkPolicyUpdate {
    pub fn begin(username: &str, ports: &[u16], allow_local_binding: bool) -> Result<Self> {
        let job = OfflinePolicyJob::open()?;
        let desired = policy_value(username, ports, allow_local_binding);
        let differs = read_policy()?.as_deref() != Some(desired.as_str());
        let active = job.active_processes()?;
        crate::logging::custody_diagnostic(|| {
            let identity = <sha2::Sha256 as sha2::Digest>::digest(desired.as_bytes());
            let identity: String = identity.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("event=policy_begin policy_id={} active={} differs={} allow_local_binding={} proxy_port_count={}",
                identity, active, differs, allow_local_binding, ports.len())
        }, None);
        // Full setup still repairs an idle compatible policy. While commands
        // are active, a compatible setup leaves their enforcement untouched.
        let needed = differs || active == 0;
        if needed {
            anyhow::ensure!(
                active == 0,
                "offline network policy is in use by running sandbox workloads; stop them before changing proxy ports or local binding"
            );
            // Invalidate before the first enforcement mutation. A failed partial
            // setup cannot let a launcher trust an old successful policy marker.
            write_policy("")?;
        }
        Ok(Self {
            _job: job,
            desired,
            needed,
        })
    }

    pub fn needed(&self) -> bool {
        self.needed
    }

    pub fn commit(self) -> Result<()> {
        let result = write_policy(&self.desired);
        crate::logging::custody_diagnostic(|| format!("event=policy_commit success={}", result.is_ok()), None);
        result
    }
}
