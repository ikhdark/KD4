//! Launch provenance, not a dependency snapshot or a shell-command trace.
use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchEnvironment {
    pub environment_id: String,
    pub cwd: String,
    /// Lexically resolved operands, not proof that traversal or reads succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_roots: Option<Vec<SearchRoot>>,
    pub launcher_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellStartup>,
    pub selected_environment: BTreeMap<String, SelectedEnvironmentValue>,
    pub environment_source: String,
    pub dependency_capture: &'static str,
    pub reproducibility: &'static str,
    pub command_coverage: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchRoot {
    pub requested: String,
    pub resolved: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectedEnvironmentValue {
    pub value: Option<String>,
    pub status: &'static str,
}

/// Identity of the created image only. A shell or executable shim does not
/// establish the identity of the program it eventually invokes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutableIdentity {
    pub resolution: &'static str,
    pub kind: &'static str,
    pub shim: &'static str,
    pub underlying_program: Option<String>,
}

impl ExecutableIdentity {
    pub(crate) fn observed(image: Option<&str>, environment: Option<&LaunchEnvironment>) -> Self {
        Self {
            resolution: if image.is_some() { "created_process_image" } else { "unknown" },
            // Only the launch owner can establish shell provenance. Neither a
            // filename nor the command payload proves program/shim identity.
            kind: if image.is_some() && environment.is_some_and(|env| env.shell.is_some()) {
                "shell_launcher"
            } else {
                "unknown"
            },
            shim: "unknown",
            underlying_program: None,
        }
    }
}

/// Final prepared shell startup, excluding the command/script payload.
/// The created image is observed separately by ProcessStart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellStartup {
    pub executable: String,
    pub startup_flags: Vec<String>,
    /// PowerShell profile startup enabled; not a claim that a profile existed.
    pub profile_loading: bool,
}

impl LaunchEnvironment {
    /// Only caller-selected entries are retained. Never enumerate ambient secrets.
    pub fn new(
        environment_id: String,
        cwd: String,
        launcher_kind: String,
        environment_source: String,
        selected: impl IntoIterator<Item = (String, Option<String>)>,
    ) -> Self {
        Self {
            environment_id,
            cwd,
            search_roots: None,
            launcher_kind,
            shell: None,
            environment_source,
            selected_environment: selected
                .into_iter()
                .map(|(name, value)| {
                    let sensitive_name = sensitive(&name);
                    let entry = match value {
                        None => SelectedEnvironmentValue {
                            value: None,
                            status: "unset",
                        },
                        Some(value) if sensitive_name || sensitive_value(&value) => {
                            SelectedEnvironmentValue {
                                value: None,
                                status: "redacted",
                            }
                        }
                        Some(value) => SelectedEnvironmentValue {
                            value: Some(value),
                            status: "captured",
                        },
                    };
                    (name, entry)
                })
                .collect(),
            dependency_capture: "not_captured",
            reproducibility: "partial",
            command_coverage: "created_root_only; shell_commands_and_descendants_not_captured",
        }
    }

    pub fn with_shell_command(mut self, command: &[String], shell_wrapper_is_owned: bool) -> Self {
        if !shell_wrapper_is_owned {
            return self;
        }
        let Some(executable) = command.first() else {
            return self;
        };
        let name = executable
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let powershell = matches!(
            name.as_str(),
            "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
        );
        let cmd = matches!(name.as_str(), "cmd" | "cmd.exe");
        if !powershell && !cmd {
            return self;
        }
        let Some(payload_flag) = command
            .iter()
            .skip(1)
            .position(|arg| {
                if powershell {
                    arg.eq_ignore_ascii_case("-Command")
                        || arg.eq_ignore_ascii_case("-EncodedCommand")
                } else {
                    arg.eq_ignore_ascii_case("/c")
                }
            })
            .map(|index| index + 1)
        else {
            return self;
        };
        let startup_flags = command[1..=payload_flag].to_vec();
        let profile_loading = powershell
            && !startup_flags
                .iter()
                .any(|arg| arg.eq_ignore_ascii_case("-NoProfile"));
        self.shell = Some(ShellStartup {
            executable: executable.clone(),
            startup_flags,
            profile_loading,
        });
        self
    }
}

fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "passwd",
        "credential",
        "auth",
        "cookie",
        "key",
        "connection",
        "proxy",
        "dsn",
        "certificate",
        "private",
    ]
    .iter()
    .any(|part| name.contains(part))
}

fn sensitive_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    // URLs can embed credentials in their authority, query, or fragment. Do not
    // try to redact just one component or emit hashes of low-entropy secrets.
    lower.contains("://")
        || lower.contains("bearer ")
        || lower.contains("-----begin")
        || lower.contains("sk-")
        || value.contains("AKIA")
        || lower.split(['=', ':']).next().is_some_and(sensitive)
}

/// Observe the created image, not a fresh PATH lookup or a parsed shell payload.
#[cfg(windows)]
pub(crate) fn executable_for_pid(pid: u32) -> Option<String> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winbase::QueryFullProcessImageNameW;
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    // SAFETY: OpenProcess returns an owned handle or null; no process mutation.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: Ownership of the successful OpenProcess result is transferred once.
    let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    // SAFETY: The handle is live and buffer has the advertised writable capacity.
    if unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle().cast(),
            0,
            buffer.as_mut_ptr(),
            &mut length,
        )
    } == 0
    {
        return None;
    }
    std::ffi::OsString::from_wide(&buffer[..length as usize])
        .into_string()
        .ok()
}

#[cfg(not(windows))]
pub(crate) fn executable_for_pid(_pid: u32) -> Option<String> {
    None
}
