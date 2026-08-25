use chrono::SecondsFormat;
use chrono::Utc;
use codex_core::DesktopPublishInstallEvidenceV1;
use std::io::Read;
use std::time::Duration;
use std::time::Instant;

const DESKTOP_ACTIVATION_EVIDENCE_HANDLE_ENV: &str = "CODEX_DESKTOP_ACTIVATION_EVIDENCE_HANDLE";
const MAX_BOOTSTRAP_EVIDENCE_BYTES: u64 = 64 * 1024;
const BOOTSTRAP_READ_TIMEOUT: Duration = Duration::from_millis(250);
const BOOTSTRAP_READ_POLL_INTERVAL: Duration = Duration::from_millis(5);

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
    consume_desktop_activation_bootstrap_from_raw_handle(raw_handle)
}

fn consume_desktop_activation_bootstrap_from_raw_handle(
    raw_handle: usize,
) -> DesktopActivationBootstrap {
    let Ok(mut pipe) = inherited_pipe_file(raw_handle) else {
        return DesktopActivationBootstrap::Malformed;
    };
    let Ok(bytes) = read_inherited_pipe_until_complete(&mut pipe, raw_handle) else {
        return DesktopActivationBootstrap::Malformed;
    };
    if bytes.is_empty() || bytes.len() as u64 > MAX_BOOTSTRAP_EVIDENCE_BYTES {
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

fn evidence_is_complete(bytes: &[u8]) -> bool {
    serde_json::from_slice::<DesktopPublishInstallEvidenceV1>(bytes).is_ok()
}

fn read_inherited_pipe_until_complete(
    pipe: &mut std::fs::File,
    raw_handle: usize,
) -> std::io::Result<Vec<u8>> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            named_pipe: *mut std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_available: *mut u32,
            bytes_left_this_message: *mut u32,
        ) -> i32;
    }

    let deadline = Instant::now() + BOOTSTRAP_READ_TIMEOUT;
    let mut bytes = Vec::new();
    loop {
        if evidence_is_complete(&bytes) || bytes.len() as u64 > MAX_BOOTSTRAP_EVIDENCE_BYTES {
            return Ok(bytes);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out reading Desktop activation bootstrap",
            ));
        }

        let mut available = 0_u32;
        // SAFETY: the handle is owned by `pipe`; null buffer arguments ask only for
        // the current byte count and do not write through the omitted pointers.
        let peeked = unsafe {
            PeekNamedPipe(
                raw_handle as *mut std::ffi::c_void,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if available == 0 {
            sleep_until_next_poll(deadline);
            continue;
        }

        let remaining =
            (MAX_BOOTSTRAP_EVIDENCE_BYTES + 1).saturating_sub(bytes.len() as u64) as usize;
        let read_len = remaining.min(available as usize);
        let mut chunk = vec![0_u8; read_len];
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn sleep_until_next_poll(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(remaining.min(BOOTSTRAP_READ_POLL_INTERVAL));
}

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

#[cfg(test)]
mod tests {
    use super::BOOTSTRAP_READ_TIMEOUT;
    use super::DesktopActivationBootstrap;
    use super::consume_desktop_activation_bootstrap_from_raw_handle;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn internal_api_visibility_is_minimal() {
        let source = include_str!("desktop_activation.rs");
        let crate_visible_declaration = [
            "pub(crate)",
            " const DESKTOP_ACTIVATION_EVIDENCE_HANDLE_ENV",
        ]
        .concat();

        assert!(
            !source.contains(&crate_visible_declaration),
            "module-local Desktop activation environment key must remain private"
        );
    }

    #[test]
    fn desktop_activation_stalled_inherited_handle_is_deadline_bounded() {
        let (raw_handle, _writer) = stalled_pipe();
        let started_at = Instant::now();

        let bootstrap = consume_desktop_activation_bootstrap_from_raw_handle(raw_handle);

        assert!(matches!(bootstrap, DesktopActivationBootstrap::Malformed));
        assert!(
            started_at.elapsed() < BOOTSTRAP_READ_TIMEOUT + Duration::from_secs(1),
            "stalled inherited pipe must not hang Desktop startup"
        );
    }

    fn stalled_pipe() -> (usize, std::fs::File) {
        use std::os::windows::io::FromRawHandle;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreatePipe(
                read_pipe: *mut *mut std::ffi::c_void,
                write_pipe: *mut *mut std::ffi::c_void,
                pipe_attributes: *const std::ffi::c_void,
                size: u32,
            ) -> i32;
        }

        let mut read_pipe = std::ptr::null_mut();
        let mut write_pipe = std::ptr::null_mut();
        // SAFETY: both output pointers are valid and null security attributes request
        // a non-inheritable anonymous pipe suitable for this ownership test.
        assert_ne!(
            unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, std::ptr::null(), 0) },
            0,
            "create stalled anonymous pipe: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: CreatePipe returned a distinct owned write handle, transferred here.
        let writer = unsafe { std::fs::File::from_raw_handle(write_pipe) };
        (read_pipe as usize, writer)
    }
}
