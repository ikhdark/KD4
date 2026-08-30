//! Agent-turn lifecycle state for `ChatWidget`.

use std::collections::HashSet;
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::iter::once;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(not(windows))]
use std::process::Child;
#[cfg(not(windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::Instant;

use tracing::warn;
#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::System::Power::POWER_REQUEST_TYPE;
#[cfg(windows)]
use windows_sys::Win32::System::Power::PowerClearRequest;
#[cfg(windows)]
use windows_sys::Win32::System::Power::PowerCreateRequest;
#[cfg(windows)]
use windows_sys::Win32::System::Power::PowerRequestSystemRequired;
#[cfg(windows)]
use windows_sys::Win32::System::Power::PowerSetRequest;
#[cfg(windows)]
use windows_sys::Win32::System::SystemServices::POWER_REQUEST_CONTEXT_VERSION;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::POWER_REQUEST_CONTEXT_SIMPLE_STRING;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::REASON_CONTEXT;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::REASON_CONTEXT_0;

const SLEEP_INHIBITOR_REASON: &str = "Codex is running an active turn";

#[derive(Debug)]
pub(super) struct SleepInhibitor {
    enabled: bool,
    turn_running: bool,
    request: Option<PowerRequest>,
}

impl SleepInhibitor {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            turn_running: false,
            request: None,
        }
    }

    fn set_turn_running(&mut self, turn_running: bool) {
        self.turn_running = turn_running;
        if !self.enabled || !turn_running {
            self.request = None;
            return;
        }
        if self.request.is_some() {
            return;
        }

        match PowerRequest::new_system_required(SLEEP_INHIBITOR_REASON) {
            Ok(request) => self.request = Some(request),
            Err(error) => {
                warn!(
                    reason = %error,
                    "Failed to acquire sleep-prevention request"
                );
            }
        }
    }

    #[cfg(test)]
    pub(super) fn is_turn_running(&self) -> bool {
        self.turn_running
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct PowerRequest {
    handle: windows_sys::Win32::Foundation::HANDLE,
    request_type: POWER_REQUEST_TYPE,
}

#[cfg(windows)]
impl PowerRequest {
    fn new_system_required(reason: &str) -> Result<Self, String> {
        let mut wide_reason: Vec<u16> = OsStr::new(reason).encode_wide().chain(once(0)).collect();
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: wide_reason.as_mut_ptr(),
            },
        };
        // SAFETY: `context` points to a valid `REASON_CONTEXT` for the duration
        // of the call and Windows copies the relevant data before returning.
        let handle = unsafe { PowerCreateRequest(&context) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            let error = std::io::Error::last_os_error();
            return Err(format!("PowerCreateRequest failed: {error}"));
        }

        let request_type = PowerRequestSystemRequired;
        // SAFETY: `handle` is a live power request handle and `request_type` is a
        // valid power request enum value.
        if unsafe { PowerSetRequest(handle, request_type) } == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: `handle` was returned by `PowerCreateRequest` and has not
            // been closed yet on this error path.
            let _ = unsafe { CloseHandle(handle) };
            return Err(format!("PowerSetRequest failed: {error}"));
        }

        Ok(Self {
            handle,
            request_type,
        })
    }
}

#[cfg(windows)]
impl Drop for PowerRequest {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is the handle owned by this `PowerRequest`, and
        // `self.request_type` is the request type that was set on acquire.
        if unsafe { PowerClearRequest(self.handle, self.request_type) } == 0 {
            let error = std::io::Error::last_os_error();
            warn!(
                reason = %error,
                "Failed to clear Windows sleep-prevention request"
            );
        }
        // SAFETY: `self.handle` is owned by this struct and closed exactly once
        // in `Drop`.
        if unsafe { CloseHandle(self.handle) } == 0 {
            let error = std::io::Error::last_os_error();
            warn!(
                reason = %error,
                "Failed to close Windows sleep-prevention request handle"
            );
        }
    }
}

#[cfg(not(windows))]
#[derive(Debug)]
struct PowerRequest {
    child: Child,
}

#[cfg(not(windows))]
impl PowerRequest {
    fn new_system_required(reason: &str) -> Result<Self, String> {
        let (program, args) = external_sleep_inhibitor_command(std::env::consts::OS)
            .ok_or_else(|| "sleep prevention is unsupported on this platform".to_string())?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if program == "systemd-inhibit" {
            command
                .arg(format!("--why={reason}"))
                .args(["sleep", "2147483647"]);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("failed to start {program}: {error}"))?;
        Ok(Self { child })
    }
}

#[cfg(not(windows))]
impl Drop for PowerRequest {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            warn!(reason = %error, "Failed to release sleep-prevention request");
        }
        let _ = self.child.wait();
    }
}

#[cfg(any(not(windows), test))]
fn external_sleep_inhibitor_command(
    target_os: &str,
) -> Option<(&'static str, &'static [&'static str])> {
    match target_os {
        "linux" => Some((
            "systemd-inhibit",
            &["--what=idle", "--mode=block", "--who=codex"],
        )),
        "macos" => Some(("caffeinate", &["-i"])),
        _ => None,
    }
}

#[derive(Debug)]
pub(super) struct TurnLifecycleState {
    pub(super) sleep_inhibitor: SleepInhibitor,
    /// Tracks whether codex-core currently considers an agent turn to be in progress.
    pub(super) agent_turn_running: bool,
    pub(super) last_turn_id: Option<String>,
    pub(super) budget_limited_turn_ids: HashSet<String>,
    pub(super) goal_status_active_turn_started_at: Option<Instant>,
}

impl TurnLifecycleState {
    pub(super) fn new(prevent_idle_sleep: bool) -> Self {
        Self {
            sleep_inhibitor: SleepInhibitor::new(prevent_idle_sleep),
            agent_turn_running: false,
            last_turn_id: None,
            budget_limited_turn_ids: HashSet::new(),
            goal_status_active_turn_started_at: None,
        }
    }

    pub(super) fn start(&mut self, now: Instant) {
        self.agent_turn_running = true;
        self.goal_status_active_turn_started_at = Some(now);
        self.sleep_inhibitor.set_turn_running(/*turn_running*/ true);
    }

    pub(super) fn finish(&mut self) {
        self.agent_turn_running = false;
        self.goal_status_active_turn_started_at = None;
        self.sleep_inhibitor
            .set_turn_running(/*turn_running*/ false);
    }

    pub(super) fn restore_running(&mut self, running: bool, now: Instant) {
        self.agent_turn_running = running;
        self.goal_status_active_turn_started_at = running.then_some(now);
        self.sleep_inhibitor.set_turn_running(running);
    }

    pub(super) fn reset_thread(&mut self) {
        self.finish();
        self.last_turn_id = None;
        self.budget_limited_turn_ids.clear();
    }

    pub(super) fn set_prevent_idle_sleep(&mut self, enabled: bool) {
        self.sleep_inhibitor = SleepInhibitor::new(enabled);
        self.sleep_inhibitor
            .set_turn_running(self.agent_turn_running);
    }

    pub(super) fn mark_budget_limited(&mut self, turn_id: String) {
        self.budget_limited_turn_ids.insert(turn_id);
    }

    pub(super) fn take_budget_limited(&mut self, turn_id: &str) -> bool {
        self.budget_limited_turn_ids.remove(turn_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_finish_update_running_state() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);

        state.start(Instant::now());
        assert!(state.agent_turn_running);
        assert!(state.goal_status_active_turn_started_at.is_some());
        assert!(state.sleep_inhibitor.is_turn_running());

        state.finish();
        assert!(!state.agent_turn_running);
        assert!(state.goal_status_active_turn_started_at.is_none());
        assert!(!state.sleep_inhibitor.is_turn_running());
    }

    #[test]
    fn budget_limited_turn_ids_are_consumed() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);

        state.mark_budget_limited("turn-1".to_string());

        assert!(state.take_budget_limited("turn-1"));
        assert!(!state.take_budget_limited("turn-1"));
    }

    #[test]
    fn changing_sleep_prevention_preserves_running_state() {
        let mut state = TurnLifecycleState::new(/*prevent_idle_sleep*/ false);
        state.restore_running(/*running*/ true, Instant::now());

        state.set_prevent_idle_sleep(/*enabled*/ false);

        assert!(state.agent_turn_running);
        assert!(state.sleep_inhibitor.is_turn_running());
    }

    #[test]
    fn external_sleep_inhibitor_commands_are_platform_specific() {
        assert_eq!(
            external_sleep_inhibitor_command("linux").map(|(program, _)| program),
            Some("systemd-inhibit")
        );
        assert_eq!(
            external_sleep_inhibitor_command("macos").map(|(program, _)| program),
            Some("caffeinate")
        );
        assert_eq!(external_sleep_inhibitor_command("windows"), None);
    }
}
