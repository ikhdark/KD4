// Rust 2024 surfaces this lint across the crate; keep the edition bump separate
// from the eventual unsafe cleanup.
#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(any(target_os = "windows", test))]
mod ssh_config_dependencies;

use std::fmt;
use std::sync::Arc;

/// Cancellation hook used by Windows sandbox capture backends.
#[derive(Clone)]
pub struct WindowsSandboxCancellationToken {
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl WindowsSandboxCancellationToken {
    /// Creates a token backed by a cancellation predicate.
    pub fn new(is_cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            is_cancelled: Arc::new(is_cancelled),
        }
    }

    /// Returns whether the caller has requested cancellation.
    pub fn is_cancelled(&self) -> bool {
        (self.is_cancelled)()
    }
}

impl fmt::Debug for WindowsSandboxCancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsSandboxCancellationToken")
            .finish_non_exhaustive()
    }
}

/// Controls whether a Windows sandbox launch reconciles persistent proxy
/// firewall settings or preserves the settings established by another launch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowsSandboxProxySettingsMode {
    #[default]
    Reconcile,
    Preserve,
}

pub const LEGACY_RESTRICTED_TOKEN_UNSAFE_DELETE_ERROR: &str = concat!(
    "legacy Windows restricted-token sandbox cannot safely enforce delete boundaries ",
    "on this Windows build; select the elevated Windows sandbox backend"
);

#[cfg(target_os = "windows")]
mod legacy_delete_child_probe_cache {
    pub(super) static LEGACY_DELETE_CHILD_RESTRICTION: std::sync::OnceLock<bool> =
        std::sync::OnceLock::new();
}

/// Returns whether the running Windows kernel applies restricting SIDs when
/// checking `FILE_DELETE_CHILD` for a write-restricted token.
///
/// A false result means the same-user legacy backend cannot safely contain
/// deletions and must fail closed rather than launch the requested process.
#[cfg(target_os = "windows")]
pub fn legacy_restricted_token_enforces_delete_child() -> bool {
    let cache = &legacy_delete_child_probe_cache::LEGACY_DELETE_CHILD_RESTRICTION;
    *cache.get_or_init(|| token::probe_legacy_delete_child_restriction().unwrap_or(false))
}

#[cfg(target_os = "windows")]
pub(crate) fn ensure_legacy_delete_child_safety(enforces_delete_child: bool) -> anyhow::Result<()> {
    if !enforces_delete_child {
        anyhow::bail!(LEGACY_RESTRICTED_TOKEN_UNSAFE_DELETE_ERROR);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
mod acl;
#[cfg(target_os = "windows")]
mod allow;
#[cfg(target_os = "windows")]
mod audit;
#[cfg(target_os = "windows")]
mod cap;
#[cfg(target_os = "windows")]
mod deny_read_acl;
#[cfg(target_os = "windows")]
mod deny_read_state;
#[cfg(target_os = "windows")]
mod desktop;
#[cfg(target_os = "windows")]
mod dpapi;
#[cfg(target_os = "windows")]
mod env;
#[cfg(target_os = "windows")]
mod helper_materialization;
#[cfg(target_os = "windows")]
mod hide_users;
#[cfg(target_os = "windows")]
mod identity;
#[cfg(target_os = "windows")]
mod logging;
#[cfg(target_os = "windows")]
mod path_normalization;
#[cfg(target_os = "windows")]
mod process;
#[cfg(target_os = "windows")]
mod resolved_permissions;
#[cfg(target_os = "windows")]
mod token;
#[cfg(target_os = "windows")]
mod wfp;
#[cfg(target_os = "windows")]
mod wfp_setup;
#[cfg(target_os = "windows")]
mod winutil;
#[cfg(target_os = "windows")]
mod workspace_acl;

mod deny_read_resolver;

#[cfg(target_os = "windows")]
mod conpty;

#[cfg(target_os = "windows")]
mod elevated;

#[cfg(target_os = "windows")]
mod elevated_impl;

#[cfg(target_os = "windows")]
mod proc_thread_attr;

#[cfg(target_os = "windows")]
mod sandbox_utils;

#[cfg(target_os = "windows")]
mod setup;

#[cfg(target_os = "windows")]
mod setup_error;

#[cfg(target_os = "windows")]
mod spawn_prep;

#[cfg(target_os = "windows")]
mod stdio_bridge;

#[cfg(target_os = "windows")]
mod unified_exec;
#[cfg(target_os = "windows")]
mod wrapper;

#[cfg(target_os = "windows")]
pub(crate) use elevated::ipc_framed;

#[cfg(target_os = "windows")]
pub(crate) use elevated::runner_client;

#[cfg(target_os = "windows")]
pub(crate) use elevated::runner_pipe;

#[cfg(target_os = "windows")]
pub use acl::add_deny_read_ace;
#[cfg(target_os = "windows")]
pub use acl::add_deny_write_ace;

#[cfg(target_os = "windows")]
pub use acl::allow_null_device;
#[cfg(target_os = "windows")]
pub use acl::ensure_allow_mask_aces;
#[cfg(target_os = "windows")]
pub use acl::ensure_allow_mask_aces_with_inheritance;
#[cfg(target_os = "windows")]
pub use acl::ensure_allow_write_aces;
#[cfg(target_os = "windows")]
pub use acl::fetch_dacl_handle;
#[cfg(target_os = "windows")]
pub use acl::path_mask_allows;
#[cfg(target_os = "windows")]
pub use audit::apply_world_writable_scan_and_denies_for_permissions;
#[cfg(target_os = "windows")]
pub use cap::load_or_create_cap_sids;
#[cfg(target_os = "windows")]
pub use cap::workspace_cap_sid_for_cwd;
#[cfg(target_os = "windows")]
pub use cap::workspace_write_cap_sid_for_root;
#[cfg(target_os = "windows")]
pub use cap::workspace_write_root_contains_path;
#[cfg(target_os = "windows")]
pub use cap::workspace_write_root_overlaps_path;
#[cfg(target_os = "windows")]
pub use conpty::ConptyInstance;
#[cfg(target_os = "windows")]
pub use conpty::spawn_conpty_process_as_user;
#[cfg(target_os = "windows")]
pub use deny_read_acl::apply_deny_read_acls;
#[cfg(target_os = "windows")]
pub use deny_read_acl::plan_deny_read_acl_paths;
pub use deny_read_resolver::resolve_windows_deny_read_paths;
#[cfg(target_os = "windows")]
pub use deny_read_state::sync_persistent_deny_read_acls;
#[cfg(target_os = "windows")]
pub use desktop::LaunchDesktop;
#[cfg(target_os = "windows")]
pub use dpapi::protect as dpapi_protect;
#[cfg(target_os = "windows")]
pub use dpapi::unprotect as dpapi_unprotect;
#[cfg(target_os = "windows")]
pub use elevated_impl::ElevatedSandboxProfileCaptureRequest;
#[cfg(target_os = "windows")]
pub use elevated_impl::run_windows_sandbox_capture_for_permission_profile as run_windows_sandbox_capture_for_permission_profile_elevated;
#[cfg(target_os = "windows")]
pub use helper_materialization::resolve_current_exe_for_launch;
#[cfg(target_os = "windows")]
pub use helper_materialization::resolve_exe_for_launch;
#[cfg(target_os = "windows")]
pub use hide_users::hide_current_user_profile_dir;
#[cfg(target_os = "windows")]
pub use hide_users::hide_newly_created_users;
#[cfg(target_os = "windows")]
pub use identity::require_logon_sandbox_creds;
#[cfg(target_os = "windows")]
pub use identity::sandbox_setup_is_complete;
#[cfg(target_os = "windows")]
pub use ipc_framed::ErrorPayload;
#[cfg(target_os = "windows")]
pub use ipc_framed::ErrorStage;
#[cfg(target_os = "windows")]
pub use ipc_framed::ExitPayload;
#[cfg(target_os = "windows")]
pub use ipc_framed::FramedMessage;
#[cfg(target_os = "windows")]
pub use ipc_framed::IPC_PROTOCOL_VERSION;
#[cfg(target_os = "windows")]
pub use ipc_framed::Message;
#[cfg(target_os = "windows")]
pub use ipc_framed::OutputPayload;
#[cfg(target_os = "windows")]
pub use ipc_framed::OutputStream;
#[cfg(target_os = "windows")]
pub use ipc_framed::ResizePayload;
#[cfg(target_os = "windows")]
pub use ipc_framed::SpawnReady;
#[cfg(target_os = "windows")]
pub use ipc_framed::SpawnRequest;
#[cfg(target_os = "windows")]
pub use ipc_framed::decode_bytes;
#[cfg(target_os = "windows")]
pub use ipc_framed::encode_bytes;
#[cfg(target_os = "windows")]
pub use ipc_framed::read_frame;
#[cfg(target_os = "windows")]
pub use ipc_framed::write_frame;
#[cfg(target_os = "windows")]
pub use logging::current_log_file_path;
#[cfg(target_os = "windows")]
pub use logging::current_log_file_path_for_codex_home;
#[cfg(target_os = "windows")]
pub use logging::log_file_path_for_utc_date;
#[cfg(target_os = "windows")]
pub use logging::log_note;
#[cfg(target_os = "windows")]
pub use logging::log_writer;
#[cfg(target_os = "windows")]
pub use path_normalization::canonicalize_path;
#[cfg(target_os = "windows")]
pub use process::PipeSpawnHandles;
#[cfg(target_os = "windows")]
pub use process::StderrMode;
#[cfg(target_os = "windows")]
pub use process::StdinMode;
#[cfg(target_os = "windows")]
pub use process::create_process_as_user;
#[cfg(target_os = "windows")]
pub use process::read_handle_loop;
#[cfg(target_os = "windows")]
pub use process::spawn_process_with_pipes;
#[cfg(target_os = "windows")]
pub use resolved_permissions::ResolvedWindowsSandboxPermissions;
#[cfg(target_os = "windows")]
pub use resolved_permissions::WindowsSandboxTokenMode;
#[cfg(target_os = "windows")]
pub use resolved_permissions::token_mode_for_permission_profile;
#[cfg(target_os = "windows")]
pub use setup::SETUP_VERSION;
#[cfg(target_os = "windows")]
pub use setup::SandboxSetupRequest;
#[cfg(target_os = "windows")]
pub use setup::SetupRootOverrides;
#[cfg(target_os = "windows")]
pub use setup::run_elevated_provisioning_setup;
#[cfg(target_os = "windows")]
pub use setup::run_elevated_setup;
#[cfg(target_os = "windows")]
pub use setup::run_setup_refresh;
#[cfg(target_os = "windows")]
pub use setup::run_setup_refresh_with_extra_read_roots;
#[cfg(target_os = "windows")]
pub use setup::sandbox_bin_dir;
#[cfg(target_os = "windows")]
pub use setup::sandbox_dir;
#[cfg(target_os = "windows")]
pub use setup::sandbox_secrets_dir;
#[cfg(target_os = "windows")]
pub use setup_error::SetupErrorCode;
#[cfg(target_os = "windows")]
pub use setup_error::SetupErrorReport;
#[cfg(target_os = "windows")]
pub use setup_error::SetupFailure;
#[cfg(target_os = "windows")]
pub use setup_error::extract_failure as extract_setup_failure;
#[cfg(target_os = "windows")]
pub use setup_error::sanitize_setup_metric_tag_value;
#[cfg(target_os = "windows")]
pub use setup_error::setup_error_path;
#[cfg(target_os = "windows")]
pub use setup_error::write_setup_error_report;
#[cfg(target_os = "windows")]
pub use stdio_bridge::forward_sandbox_session_stdio;
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub use token::LocalSid;
#[cfg(target_os = "windows")]
pub use token::convert_string_sid_to_sid;
#[cfg(target_os = "windows")]
pub use token::create_readonly_token_with_cap_from;
#[cfg(target_os = "windows")]
pub use token::create_readonly_token_with_caps_and_user_from;
#[cfg(target_os = "windows")]
pub use token::create_readonly_token_with_caps_from;
#[cfg(target_os = "windows")]
pub use token::create_workspace_write_token_with_caps_and_user_from;
#[cfg(target_os = "windows")]
pub use token::create_workspace_write_token_with_caps_from;
#[cfg(target_os = "windows")]
pub use token::get_current_token_for_restriction;
#[cfg(target_os = "windows")]
pub use unified_exec::WindowsSandboxSessionRequest;
#[cfg(target_os = "windows")]
pub use unified_exec::spawn_windows_sandbox_session_elevated_for_permission_profile;
#[cfg(target_os = "windows")]
pub use unified_exec::spawn_windows_sandbox_session_for_level;
#[cfg(target_os = "windows")]
pub use unified_exec::spawn_windows_sandbox_session_legacy;
#[cfg(target_os = "windows")]
pub use wfp::install_wfp_filters_for_account;
#[cfg(target_os = "windows")]
pub use wfp_setup::install_wfp_filters;
#[cfg(target_os = "windows")]
pub use windows_impl::CaptureResult;
#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture;
#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture_with_filesystem_overrides;
#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_legacy_preflight;
#[cfg(target_os = "windows")]
pub use winutil::quote_windows_arg;
#[cfg(target_os = "windows")]
pub use winutil::string_from_sid_bytes;
#[cfg(target_os = "windows")]
pub use winutil::to_wide;
#[cfg(target_os = "windows")]
pub use workspace_acl::is_command_cwd_root;
#[cfg(target_os = "windows")]
pub use wrapper::CODEX_WINDOWS_SANDBOX_ARG1;
#[cfg(target_os = "windows")]
pub use wrapper::create_windows_sandbox_command_args_for_permission_profile;
#[cfg(target_os = "windows")]
pub use wrapper::run_windows_sandbox_wrapper_main;

#[cfg(not(target_os = "windows"))]
pub use stub::CaptureResult;
#[cfg(not(target_os = "windows"))]
pub use stub::run_windows_sandbox_capture;
#[cfg(not(target_os = "windows"))]
pub use stub::run_windows_sandbox_legacy_preflight;

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::WindowsSandboxCancellationToken;
    use super::legacy_restricted_token_enforces_delete_child;
    use super::logging::log_failure;
    use super::logging::log_success;
    use super::process::create_process_as_user;
    use super::sandbox_utils::ensure_codex_home_exists;
    use super::spawn_prep::LegacyAclSids;
    use super::spawn_prep::SpawnPrepOptions;
    use super::spawn_prep::allow_null_device_for_workspace_write;
    use super::spawn_prep::apply_legacy_session_acl_rules;
    use super::spawn_prep::legacy_session_capability_roots;
    use super::spawn_prep::prepare_legacy_session_security;
    use super::spawn_prep::prepare_legacy_spawn_context;
    use super::spawn_prep::root_capability_sids;
    use anyhow::Result;
    use codex_protocol::models::PermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::collections::HashMap;
    use std::io;
    use std::path::Path;
    use std::ptr;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Foundation::ERROR_NO_DATA;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;
    use windows_sys::Win32::System::Pipes::SetNamedPipeHandleState;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::INFINITE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    type PipeHandles = ((HANDLE, HANDLE), (HANDLE, HANDLE), (HANDLE, HANDLE));

    const CAPTURE_PIPE_POLL_INTERVAL: Duration = Duration::from_millis(10);
    const CAPTURE_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

    enum WaitOutcome {
        Exited,
        TimedOut,
        Cancelled,
    }

    fn wait_for_process(
        process: HANDLE,
        timeout_ms: Option<u64>,
        cancellation: Option<&WindowsSandboxCancellationToken>,
    ) -> WaitOutcome {
        let Some(cancellation) = cancellation else {
            let timeout = timeout_ms.map(|ms| ms as u32).unwrap_or(INFINITE);
            let res = unsafe { WaitForSingleObject(process, timeout) };
            return if res == 0x0000_0102 {
                WaitOutcome::TimedOut
            } else {
                WaitOutcome::Exited
            };
        };

        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            if cancellation.is_cancelled() {
                return WaitOutcome::Cancelled;
            }
            let wait_ms = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return WaitOutcome::TimedOut;
                    }
                    remaining.min(Duration::from_millis(50)).as_millis() as u32
                }
                None => 50,
            };
            let res = unsafe { WaitForSingleObject(process, wait_ms) };
            if res == 0x0000_0102 {
                continue;
            }
            return WaitOutcome::Exited;
        }
    }

    unsafe fn close_pipe_handles(handles: &[HANDLE]) {
        for &handle in handles {
            if handle != 0 && handle != INVALID_HANDLE_VALUE {
                CloseHandle(handle);
            }
        }
    }

    unsafe fn pipe_setup_error(handles: &[HANDLE]) -> io::Error {
        let error = GetLastError();
        close_pipe_handles(handles);
        io::Error::from_raw_os_error(error as i32)
    }

    unsafe fn setup_stdio_pipes() -> io::Result<PipeHandles> {
        let mut in_r: HANDLE = 0;
        let mut in_w: HANDLE = 0;
        let mut out_r: HANDLE = 0;
        let mut out_w: HANDLE = 0;
        let mut err_r: HANDLE = 0;
        let mut err_w: HANDLE = 0;
        if CreatePipe(&mut in_r, &mut in_w, ptr::null_mut(), 0) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if CreatePipe(&mut out_r, &mut out_w, ptr::null_mut(), 0) == 0 {
            return Err(pipe_setup_error(&[in_r, in_w]));
        }
        if CreatePipe(&mut err_r, &mut err_w, ptr::null_mut(), 0) == 0 {
            return Err(pipe_setup_error(&[in_r, in_w, out_r, out_w]));
        }
        let handles = [in_r, in_w, out_r, out_w, err_r, err_w];
        if SetHandleInformation(in_r, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(pipe_setup_error(&handles));
        }
        if SetHandleInformation(out_w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(pipe_setup_error(&handles));
        }
        if SetHandleInformation(err_w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(pipe_setup_error(&handles));
        }
        let pipe_mode = PIPE_NOWAIT;
        if SetNamedPipeHandleState(out_r, &pipe_mode, ptr::null(), ptr::null()) == 0 {
            return Err(pipe_setup_error(&handles));
        }
        if SetNamedPipeHandleState(err_r, &pipe_mode, ptr::null(), ptr::null()) == 0 {
            return Err(pipe_setup_error(&handles));
        }
        Ok(((in_r, in_w), (out_r, out_w), (err_r, err_w)))
    }

    pub struct CaptureResult {
        pub exit_code: i32,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
        pub timed_out: bool,
    }

    struct OwnedCapturePipeHandle(HANDLE);

    impl Drop for OwnedCapturePipeHandle {
        fn drop(&mut self) {
            if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn read_capture_pipe(handle: HANDLE, stop_rx: mpsc::Receiver<()>) -> io::Result<Vec<u8>> {
        let _handle = OwnedCapturePipeHandle(handle);
        let mut output = Vec::new();
        let mut tmp = [0u8; 8192];
        let mut drain_deadline = None;

        loop {
            if drain_deadline.is_none() {
                match stop_rx.try_recv() {
                    Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                        drain_deadline = Some(Instant::now() + self::CAPTURE_PIPE_DRAIN_TIMEOUT);
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if drain_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }

            let mut read_bytes = 0u32;
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    handle,
                    tmp.as_mut_ptr(),
                    tmp.len() as u32,
                    &mut read_bytes,
                    ptr::null_mut(),
                )
            };
            let no_data = if ok != 0 {
                if read_bytes == 0 {
                    true
                } else {
                    output.extend_from_slice(&tmp[..read_bytes as usize]);
                    false
                }
            } else {
                let error = unsafe { GetLastError() };
                match error {
                    ERROR_BROKEN_PIPE => break,
                    ERROR_NO_DATA => true,
                    _ => return Err(io::Error::from_raw_os_error(error as i32)),
                }
            };

            if let Some(deadline) = drain_deadline {
                if no_data || Instant::now() >= deadline {
                    break;
                }
                continue;
            }
            if no_data {
                match stop_rx.recv_timeout(self::CAPTURE_PIPE_POLL_INTERVAL) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        drain_deadline = Some(Instant::now() + self::CAPTURE_PIPE_DRAIN_TIMEOUT);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }

        Ok(output)
    }

    struct CapturePipeReader {
        stop_tx: mpsc::SyncSender<()>,
        join: Option<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
    }

    impl CapturePipeReader {
        fn spawn_capture_pipe_reader(handle: HANDLE) -> Self {
            let (stop_tx, stop_rx) = mpsc::sync_channel(1);
            let join = std::thread::spawn(move || read_capture_pipe(handle, stop_rx));
            Self {
                stop_tx,
                join: Some(join),
            }
        }

        fn request_stop(&self) {
            let _ = self.stop_tx.try_send(());
        }

        fn join_reader(&mut self) -> io::Result<Vec<u8>> {
            let Some(join) = self.join.take() else {
                return Err(io::Error::other("capture pipe reader already joined"));
            };
            match join.join() {
                Ok(result) => result,
                Err(_) => Err(io::Error::other("capture pipe reader thread panicked")),
            }
        }

        fn stop_and_collect(mut self) -> io::Result<Vec<u8>> {
            self.request_stop();
            self.join_reader()
        }
    }

    impl Drop for CapturePipeReader {
        fn drop(&mut self) {
            self.request_stop();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
        codex_home: &Path,
        command: Vec<String>,
        cwd: &Path,
        env_map: HashMap<String, String>,
        timeout_ms: Option<u64>,
        cancellation: Option<WindowsSandboxCancellationToken>,
        use_private_desktop: bool,
    ) -> Result<CaptureResult> {
        run_windows_sandbox_capture_with_filesystem_overrides(
            permission_profile,
            workspace_roots,
            codex_home,
            command,
            cwd,
            env_map,
            timeout_ms,
            cancellation,
            &[],
            &[],
            use_private_desktop,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture_with_filesystem_overrides(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
        codex_home: &Path,
        command: Vec<String>,
        cwd: &Path,
        mut env_map: HashMap<String, String>,
        timeout_ms: Option<u64>,
        cancellation: Option<WindowsSandboxCancellationToken>,
        additional_deny_read_paths: &[AbsolutePathBuf],
        additional_deny_write_paths: &[AbsolutePathBuf],
        use_private_desktop: bool,
    ) -> Result<CaptureResult> {
        super::ensure_legacy_delete_child_safety(legacy_restricted_token_enforces_delete_child())?;
        let additional_deny_read_paths = additional_deny_read_paths
            .iter()
            .map(AbsolutePathBuf::to_path_buf)
            .collect::<Vec<_>>();
        let additional_deny_write_paths = additional_deny_write_paths
            .iter()
            .map(AbsolutePathBuf::to_path_buf)
            .collect::<Vec<_>>();
        let common = prepare_legacy_spawn_context(
            permission_profile,
            workspace_roots,
            codex_home,
            cwd,
            &mut env_map,
            &command,
            SpawnPrepOptions {
                inherit_path: false,
                add_git_safe_directory: false,
            },
        )?;
        let permissions = common.permissions;
        let current_dir = common.current_dir;
        let logs_base_dir = common.logs_base_dir.as_deref();
        let uses_write_capabilities = common.uses_write_capabilities;
        if !permissions.has_full_disk_read_access() {
            anyhow::bail!(
                "Restricted read-only access requires the elevated Windows sandbox backend"
            );
        }
        // WRITE_RESTRICTED tokens consult restricting SIDs only for writes, so this
        // backend cannot make capability-SID deny-read ACLs authoritative.
        if !additional_deny_read_paths.is_empty() {
            anyhow::bail!("deny-read overrides require the elevated Windows sandbox backend");
        }
        let capability_roots =
            legacy_session_capability_roots(&permissions, &current_dir, &env_map, codex_home);
        let security = prepare_legacy_session_security(
            uses_write_capabilities,
            codex_home,
            cwd,
            capability_roots,
        )?;
        allow_null_device_for_workspace_write(uses_write_capabilities);
        apply_legacy_session_acl_rules(
            &permissions,
            codex_home,
            &current_dir,
            &env_map,
            &additional_deny_read_paths,
            &additional_deny_write_paths,
            LegacyAclSids {
                readonly_sid: security.readonly_sid.as_ref(),
                readonly_sid_str: security.readonly_sid_str.as_deref(),
                write_root_sids: &security.write_root_sids,
            },
        )?;
        let (stdin_pair, stdout_pair, stderr_pair) = unsafe { setup_stdio_pipes()? };
        let ((in_r, in_w), (out_r, out_w), (err_r, err_w)) = (stdin_pair, stdout_pair, stderr_pair);
        let spawn_res = unsafe {
            create_process_as_user(
                security.h_token,
                &command,
                cwd,
                &env_map,
                logs_base_dir,
                Some((in_r, out_w, err_w)),
                use_private_desktop,
            )
        };
        let created = match spawn_res {
            Ok(v) => v,
            Err(err) => {
                unsafe {
                    CloseHandle(in_r);
                    CloseHandle(in_w);
                    CloseHandle(out_r);
                    CloseHandle(out_w);
                    CloseHandle(err_r);
                    CloseHandle(err_w);
                    CloseHandle(security.h_token);
                }
                return Err(err);
            }
        };
        let pi = created.process_info;
        let _desktop = created;

        unsafe {
            CloseHandle(in_r);
            // Close the parent's stdin write end so the child sees EOF immediately.
            CloseHandle(in_w);
            CloseHandle(out_w);
            CloseHandle(err_w);
        }

        let stdout_reader = CapturePipeReader::spawn_capture_pipe_reader(out_r);
        let stderr_reader = CapturePipeReader::spawn_capture_pipe_reader(err_r);

        let wait_outcome = wait_for_process(pi.hProcess, timeout_ms, cancellation.as_ref());
        let timed_out = matches!(wait_outcome, WaitOutcome::TimedOut);
        let cancelled = matches!(wait_outcome, WaitOutcome::Cancelled);
        let mut exit_code_u32: u32 = 1;
        if !timed_out && !cancelled {
            unsafe {
                GetExitCodeProcess(pi.hProcess, &mut exit_code_u32);
            }
        } else {
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                let _ = WaitForSingleObject(pi.hProcess, 5_000);
            }
        }

        unsafe {
            if pi.hThread != 0 {
                CloseHandle(pi.hThread);
            }
            if pi.hProcess != 0 {
                CloseHandle(pi.hProcess);
            }
            CloseHandle(security.h_token);
        }
        let stdout_result = stdout_reader.stop_and_collect();
        let stderr_result = stderr_reader.stop_and_collect();
        let stdout = stdout_result?;
        let stderr = stderr_result?;
        let exit_code = if timed_out {
            128 + 64
        } else {
            exit_code_u32 as i32
        };

        if exit_code == 0 {
            log_success(&command, logs_base_dir);
        } else {
            log_failure(&command, &format!("exit code {exit_code}"), logs_base_dir);
        }

        Ok(CaptureResult {
            exit_code,
            stdout,
            stderr,
            timed_out,
        })
    }

    pub fn run_windows_sandbox_legacy_preflight(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
        codex_home: &Path,
        cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> Result<()> {
        let Ok(permissions) = super::resolved_permissions::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        ) else {
            return Ok(());
        };
        super::ensure_legacy_delete_child_safety(legacy_restricted_token_enforces_delete_child())?;
        if !permissions.uses_write_capabilities_for_cwd(cwd, env_map) {
            return Ok(());
        }

        ensure_codex_home_exists(codex_home)?;
        let current_dir = cwd.to_path_buf();
        let capability_roots =
            legacy_session_capability_roots(&permissions, &current_dir, env_map, codex_home);
        let write_root_sids = root_capability_sids(codex_home, cwd, capability_roots)?;
        apply_legacy_session_acl_rules(
            &permissions,
            codex_home,
            &current_dir,
            env_map,
            &[],
            &[],
            LegacyAclSids {
                readonly_sid: None,
                readonly_sid_str: None,
                write_root_sids: &write_root_sids,
            },
        )?;

        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
        use codex_protocol::models::PermissionProfile;
        use codex_protocol::permissions::NetworkSandboxPolicy;
        use std::collections::HashMap;
        use std::io;
        use std::path::Path;
        use std::ptr;
        use std::time::Duration;
        use std::time::Instant;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        fn workspace_profile(network_policy: NetworkSandboxPolicy) -> PermissionProfile {
            PermissionProfile::workspace_write_with(
                &[],
                network_policy,
                /*exclude_tmpdir_env_var*/ false,
                /*exclude_slash_tmp*/ false,
            )
        }

        fn should_apply_network_block(permission_profile: &PermissionProfile) -> bool {
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                permission_profile,
                &[],
            )
            .expect("managed permissions")
            .should_apply_network_block()
        }

        #[test]
        fn applies_network_block_when_access_is_disabled() {
            assert!(should_apply_network_block(&workspace_profile(
                NetworkSandboxPolicy::Restricted
            )));
        }

        #[test]
        fn skips_network_block_when_access_is_allowed() {
            assert!(!should_apply_network_block(&workspace_profile(
                NetworkSandboxPolicy::Enabled
            )));
        }

        #[test]
        fn applies_network_block_for_read_only() {
            assert!(should_apply_network_block(&PermissionProfile::read_only()));
        }

        fn capture_pipe_with_open_writer() -> (super::CapturePipeReader, HANDLE) {
            let ((in_r, in_w), (out_r, out_w), (err_r, err_w)) =
                unsafe { super::setup_stdio_pipes() }.expect("create capture pipes");
            unsafe {
                CloseHandle(in_r);
                CloseHandle(in_w);
                CloseHandle(err_r);
                CloseHandle(err_w);
            }
            (
                super::CapturePipeReader::spawn_capture_pipe_reader(out_r),
                out_w,
            )
        }

        fn write_pipe(handle: HANDLE, mut bytes: &[u8]) -> io::Result<()> {
            while !bytes.is_empty() {
                let mut written = 0u32;
                let ok = unsafe {
                    WriteFile(
                        handle,
                        bytes.as_ptr(),
                        bytes.len() as u32,
                        &mut written,
                        ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                if written == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pipe write completed without writing bytes",
                    ));
                }
                bytes = &bytes[written as usize..];
            }
            Ok(())
        }

        #[test]
        fn stopped_capture_reader_joins_before_return_with_writer_open() {
            let started_at = Instant::now();
            for _ in 0..16 {
                let (reader, out_w) = capture_pipe_with_open_writer();
                let output = reader.stop_and_collect();
                let write_after_join = write_pipe(out_w, b"x");
                unsafe {
                    CloseHandle(out_w);
                }

                assert!(output.expect("stop and join capture reader").is_empty());
                assert!(
                    write_after_join.is_err(),
                    "reader handle must be closed before collection returns"
                );
            }

            assert!(
                started_at.elapsed() < Duration::from_secs(2),
                "repeated stop-and-join cycles should be bounded"
            );
        }

        #[test]
        fn stopped_capture_reader_preserves_buffered_output() {
            let (reader, out_w) = capture_pipe_with_open_writer();
            let expected = b"buffered before stop";
            write_pipe(out_w, expected).expect("buffer output");

            let output = reader.stop_and_collect();
            unsafe {
                CloseHandle(out_w);
            }

            assert_eq!(
                output.expect("stop and join capture reader").as_slice(),
                expected
            );
        }

        #[test]
        fn stopped_capture_reader_bounds_continuous_writer_drain() {
            let (reader, out_w) = capture_pipe_with_open_writer();
            let writer = std::thread::spawn(move || {
                let chunk = [b'x'; 64];
                while write_pipe(out_w, &chunk).is_ok() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                unsafe {
                    CloseHandle(out_w);
                }
            });
            std::thread::sleep(Duration::from_millis(20));

            let started_at = Instant::now();
            let output = reader.stop_and_collect();
            let elapsed = started_at.elapsed();
            writer.join().expect("join capture writer");

            assert!(!output.expect("stop and join capture reader").is_empty());
            assert!(
                elapsed < Duration::from_secs(1),
                "continuous output must not extend the bounded drain"
            );
        }

        #[test]
        fn capture_reader_propagates_read_errors() {
            let reader = super::CapturePipeReader::spawn_capture_pipe_reader(INVALID_HANDLE_VALUE);
            let err = reader
                .stop_and_collect()
                .expect_err("invalid pipe handle should fail");

            assert_eq!(err.raw_os_error(), Some(ERROR_INVALID_HANDLE as i32));
        }

        #[test]
        fn legacy_preflight_skips_profiles_without_managed_filesystem_permissions() {
            for permission_profile in [
                PermissionProfile::Disabled,
                PermissionProfile::External {
                    network: NetworkSandboxPolicy::Restricted,
                },
            ] {
                super::run_windows_sandbox_legacy_preflight(
                    &permission_profile,
                    &[],
                    Path::new("."),
                    Path::new("."),
                    &HashMap::new(),
                )
                .expect("unsupported profiles do not need ACL preflight");
            }
        }

        #[test]
        fn legacy_delete_child_safety_is_deterministic() {
            crate::ensure_legacy_delete_child_safety(true)
                .expect("safe delete-child semantics should be accepted");
            let err = crate::ensure_legacy_delete_child_safety(false)
                .expect_err("unsafe delete-child semantics should fail closed");
            assert_eq!(
                err.to_string(),
                crate::LEGACY_RESTRICTED_TOKEN_UNSAFE_DELETE_ERROR
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::WindowsSandboxCancellationToken;
    use anyhow::Result;
    use anyhow::bail;
    use codex_protocol::models::PermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::collections::HashMap;
    use std::path::Path;

    #[derive(Debug, Default)]
    pub struct CaptureResult {
        pub exit_code: i32,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
        pub timed_out: bool,
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture(
        _permission_profile: &PermissionProfile,
        _workspace_roots: &[AbsolutePathBuf],
        _codex_home: &Path,
        _command: Vec<String>,
        _cwd: &Path,
        _env_map: HashMap<String, String>,
        _timeout_ms: Option<u64>,
        _cancellation: Option<WindowsSandboxCancellationToken>,
        _use_private_desktop: bool,
    ) -> Result<CaptureResult> {
        bail!("Windows sandbox is only available on Windows")
    }

    pub fn run_windows_sandbox_legacy_preflight(
        _permission_profile: &PermissionProfile,
        _workspace_roots: &[AbsolutePathBuf],
        _codex_home: &Path,
        _cwd: &Path,
        _env_map: &HashMap<String, String>,
    ) -> Result<()> {
        bail!("Windows sandbox is only available on Windows")
    }
}
