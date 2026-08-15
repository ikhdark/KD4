use chrono::SecondsFormat;
use chrono::Utc;
use codex_core::DesktopPublishInstallEvidenceV1;
use std::io::Read;

pub(crate) const DESKTOP_ACTIVATION_EVIDENCE_HANDLE_ENV: &str =
    "CODEX_DESKTOP_ACTIVATION_EVIDENCE_HANDLE";
const MAX_BOOTSTRAP_EVIDENCE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) enum DesktopActivationBootstrap {
    Absent,
    Malformed,
    Available {
        evidence: Box<DesktopPublishInstallEvidenceV1>,
        consumed_at: String,
    },
}

/// Consume and close the publisher-owned one-shot pipe before request handling starts.
pub(crate) fn consume_desktop_activation_bootstrap() -> DesktopActivationBootstrap {
    let Some(raw_handle) = std::env::var_os(DESKTOP_ACTIVATION_EVIDENCE_HANDLE_ENV) else {
        return DesktopActivationBootstrap::Absent;
    };
    let Some(raw_handle) = raw_handle.to_str() else {
        return DesktopActivationBootstrap::Malformed;
    };
    let Ok(raw_handle) = raw_handle.parse::<usize>() else {
        return DesktopActivationBootstrap::Malformed;
    };
    if raw_handle == 0 {
        return DesktopActivationBootstrap::Malformed;
    }
    let Ok(mut pipe) = inherited_pipe_file(raw_handle) else {
        return DesktopActivationBootstrap::Malformed;
    };
    let mut bytes = Vec::new();
    if pipe
        .by_ref()
        .take(MAX_BOOTSTRAP_EVIDENCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.is_empty()
        || bytes.len() as u64 > MAX_BOOTSTRAP_EVIDENCE_BYTES
    {
        return DesktopActivationBootstrap::Malformed;
    }
    let Ok(evidence) = serde_json::from_slice::<DesktopPublishInstallEvidenceV1>(&bytes) else {
        return DesktopActivationBootstrap::Malformed;
    };
    DesktopActivationBootstrap::Available {
        evidence: Box::new(evidence),
        consumed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
    }
}

#[cfg(windows)]
fn inherited_pipe_file(raw_handle: usize) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::SetHandleInformation;

    let handle = raw_handle as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: the native parent supplied this inherited handle. On success `File` owns it.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: inheritance removal succeeded and ownership transfers to this one `File`.
    Ok(unsafe { std::fs::File::from_raw_handle(raw_handle as *mut std::ffi::c_void) })
}

#[cfg(unix)]
fn inherited_pipe_file(raw_handle: usize) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;

    let fd = i32::try_from(raw_handle)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid pipe fd"))?;
    // SAFETY: the native parent supplied this inherited one-shot read descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(not(any(windows, unix)))]
fn inherited_pipe_file(_raw_handle: usize) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Desktop activation bootstrap handles are unsupported",
    ))
}
