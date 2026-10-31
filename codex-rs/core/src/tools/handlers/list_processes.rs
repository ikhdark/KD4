//! Local process inventory without a shell. Recorded sessions spent 156
//! `Get-CimInstance Win32_Process` pipelines in 2.5 days on one question:
//! which cargo/rustc/python processes are running, with which command line.
//! A Toolhelp snapshot supplies ids, parents and image names; limited-
//! information handles add path, start/CPU times, working set and, on
//! request, the command line. Read-only: no process is signaled.
use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct ListProcessesHandler;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_FILTER_ITEMS: usize = 64;
const MAX_COMMAND_LINE_CHARS: usize = 4_096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    pids: Vec<u32>,
    parent_pid: Option<u32>,
    command_line_regex: Option<String>,
    #[serde(default)]
    include_command_line: bool,
    limit: Option<usize>,
}

struct Filter {
    names: Vec<String>,
    pids: Vec<u32>,
    parent_pid: Option<u32>,
    command_line: Option<regex::Regex>,
    include_command_line: bool,
    limit: usize,
}

impl ToolExecutor<ToolInvocation> for ListProcessesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_processes")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let strings = |description: &str| JsonSchema::array(
            JsonSchema::string(None),
            Some(description.into()),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: "list_processes".into(),
            description: "List processes on this local Windows host without starting a shell: a Toolhelp snapshot plus limited-information process handles. Filters combine with AND; entries within names or pids combine with OR. Command lines are read only for command_line_regex or include_command_line. Protected or exited processes can lack path, times, memory or command line; they are counted, and with command_line_regex they cannot match, so absence among them is unknown. Point-in-time and read-only: processes can exit and ids be reused afterwards, parent_pid may name an exited or reused process, and nothing is signaled or stopped.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("names".into(), strings("Image names, case-insensitive; .exe is optional and * matches any run (python, rustc.exe, wall-clock-*). At most 64.")),
                ("pids".into(), JsonSchema::array(JsonSchema::integer(None), Some("Process ids. At most 64.".into()))),
                ("parent_pid".into(), JsonSchema::integer(Some("Only direct children of this process id.".into()))),
                ("command_line_regex".into(), JsonSchema::string(Some("Case-insensitive Rust regex searched in each command line (benchmark_|kdr_session_suite).".into()))),
                ("include_command_line".into(), JsonSchema::boolean(Some("Return command lines (each capped at 4096 characters). Implied by command_line_regex. Default false.".into()))),
                ("limit".into(), JsonSchema::integer(Some("Maximum processes returned, ordered by pid; default 100, at most 500. matched counts all.".into()))),
            ]), None, Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "observed_at":{"type":"string"}, "current_process_id":{"type":"integer"},
                    "matched":{"type":"integer","minimum":0}, "truncated":{"type":"boolean"},
                    "details_unavailable":{"type":"integer","minimum":0, "description":"Returned processes whose path, times or memory could not be read."},
                    "command_line_unavailable":{"type":"integer","minimum":0, "description":"Processes passing the other filters whose command line could not be read; never matched by command_line_regex."},
                    "processes":{"type":"array","items":{"type":"object","additionalProperties":false,
                        "properties":{
                            "pid":{"type":"integer"}, "parent_pid":{"type":"integer"}, "name":{"type":"string"},
                            "threads":{"type":"integer"}, "path":{"type":["string","null"]},
                            "started_at":{"type":["string","null"]}, "cpu_seconds":{"type":["number","null"]},
                            "working_set_bytes":{"type":["integer","null"]},
                            "command_line":{"type":["string","null"]}, "command_line_truncated":{"const":true}
                        },
                        "required":["pid","parent_pid","name","threads","path","started_at","cpu_seconds","working_set_bytes"]}}
                },
                "required":["observed_at","current_process_id","matched","truncated","details_unavailable","command_line_unavailable","processes"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "list_processes requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            // Registration requires the same gate; host processes are not an
            // inventory of a remote or read-restricted environment.
            if !invocation
                .step_context
                .environments
                .turn_environments
                .iter()
                .any(|environment| !environment.environment.is_remote())
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel(
                    "list_processes requires a local environment with full disk read access".into(),
                ));
            }
            if args.names.len() > MAX_FILTER_ITEMS || args.pids.len() > MAX_FILTER_ITEMS {
                return Err(FunctionCallError::RespondToModel(format!(
                    "list_processes accepts at most {MAX_FILTER_ITEMS} names and {MAX_FILTER_ITEMS} pids"
                )));
            }
            let command_line = args
                .command_line_regex
                .as_deref()
                .map(|pattern| {
                    regex::RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "invalid command_line_regex: {error}"
                            ))
                        })
                })
                .transpose()?;
            let filter = Filter {
                names: args.names.iter().map(|name| name.to_lowercase()).collect(),
                pids: args.pids,
                parent_pid: args.parent_pid,
                command_line,
                include_command_line: args.include_command_line,
                limit: args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            };
            let value = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("process listing cancelled".into())),
                result = tokio::task::spawn_blocking(move || list(&filter)) => result
                    .map_err(|error| FunctionCallError::RespondToModel(format!("process listing worker failed: {error}")))?
                    .map_err(FunctionCallError::RespondToModel)?,
            };
            let output = JsonToolOutput::new(value);
            if crate::tools::context::ensure_exact_model_delivery(&output, &invocation).is_err() {
                return Err(FunctionCallError::RespondToModel(
                    "list_processes output exceeds the model delivery budget; lower limit, add filters or omit include_command_line".into(),
                ));
            }
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for ListProcessesHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}

fn name_matches(pattern: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    glob(pattern, &name) || glob(pattern, stem)
}

/// `*` matches any run of characters; everything else is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts = parts.collect::<Vec<_>>();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

struct Entry {
    pid: u32,
    parent_pid: u32,
    name: String,
    threads: u32,
}

#[derive(Default)]
struct Details {
    path: Option<String>,
    started_at: Option<String>,
    cpu_seconds: Option<f64>,
    working_set_bytes: Option<u64>,
    command_line: Option<String>,
}

fn list(filter: &Filter) -> Result<serde_json::Value, String> {
    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut entries = platform::snapshot()?
        .into_iter()
        .filter(|entry| {
            (filter.pids.is_empty() || filter.pids.contains(&entry.pid))
                && filter.parent_pid.is_none_or(|parent| entry.parent_pid == parent)
                && (filter.names.is_empty()
                    || filter.names.iter().any(|pattern| name_matches(pattern, &entry.name)))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.pid);
    let read_command_line = filter.include_command_line || filter.command_line.is_some();
    let mut matched = 0usize;
    let mut details_unavailable = 0usize;
    let mut command_line_unavailable = 0usize;
    let mut processes = Vec::new();
    for entry in entries {
        // The details of an unreturned process are never needed unless its
        // command line decides whether it matches.
        if processes.len() >= filter.limit && filter.command_line.is_none() {
            matched += 1;
            continue;
        }
        let details = platform::details(entry.pid, read_command_line);
        if read_command_line && details.command_line.is_none() {
            command_line_unavailable += 1;
        }
        if let Some(regex) = &filter.command_line
            && !details.command_line.as_deref().is_some_and(|line| regex.is_match(line))
        {
            continue;
        }
        matched += 1;
        if processes.len() >= filter.limit {
            continue;
        }
        if details.path.is_none() || details.started_at.is_none() {
            details_unavailable += 1;
        }
        let mut process = json!({
            "pid":entry.pid, "parent_pid":entry.parent_pid, "name":entry.name,
            "threads":entry.threads, "path":details.path, "started_at":details.started_at,
            "cpu_seconds":details.cpu_seconds, "working_set_bytes":details.working_set_bytes,
        });
        if read_command_line {
            let line = details.command_line.map(|line| {
                if line.chars().count() > MAX_COMMAND_LINE_CHARS {
                    process["command_line_truncated"] = true.into();
                    line.chars().take(MAX_COMMAND_LINE_CHARS).collect()
                } else {
                    line
                }
            });
            process["command_line"] = json!(line);
        }
        processes.push(process);
    }
    Ok(json!({
        "observed_at":observed_at, "current_process_id":std::process::id(),
        "matched":matched, "truncated":matched > processes.len(),
        "details_unavailable":details_unavailable,
        "command_line_unavailable":command_line_unavailable, "processes":processes,
    }))
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32NextW;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
    use windows_sys::Win32::System::ProcessStatus::K32GetProcessMemoryInfo;
    use windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_NAME_WIN32;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

    use super::Details;
    use super::Entry;

    // ProcessCommandLineInformation (Windows 8.1+) needs only a limited-
    // information handle, unlike reading another process's PEB. Declared
    // here so the build does not need a new windows-sys feature.
    const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process: HANDLE,
            class: i32,
            information: *mut core::ffi::c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned open by a successful Win32 call
            // and is closed exactly once here.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn snapshot() -> Result<Vec<Entry>, String> {
        // SAFETY: plain Win32 call; the result is checked before use.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return Err(format!("process snapshot failed: {}", std::io::Error::last_os_error()));
        }
        let snapshot = Handle(snapshot);
        // SAFETY: PROCESSENTRY32W is plain data; dwSize is set as required.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut entries = Vec::new();
        // SAFETY: the snapshot handle is open and `entry` is initialized.
        let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while more {
            let length = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
            entries.push(Entry {
                pid: entry.th32ProcessID,
                parent_pid: entry.th32ParentProcessID,
                name: String::from_utf16_lossy(&entry.szExeFile[..length]),
                threads: entry.cntThreads,
            });
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        if entries.is_empty() {
            return Err(format!("process snapshot was empty: {}", std::io::Error::last_os_error()));
        }
        Ok(entries)
    }

    pub(super) fn details(pid: u32, command_line: bool) -> Details {
        // SAFETY: plain Win32 call; a null handle means no access.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Details::default();
        }
        let handle = Handle(handle);
        let mut details = Details::default();
        let mut path = vec![0u16; 32_768];
        let mut size = path.len() as u32;
        // SAFETY: the buffer holds `size` UTF-16 units.
        if unsafe { QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut size) } != 0 {
            details.path = Some(String::from_utf16_lossy(&path[..size as usize]));
        }
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: four valid FILETIME out-pointers.
        if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } != 0 {
            let ticks = |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
            // FILETIME counts 100 ns intervals from 1601-01-01.
            let unix_ms = (ticks(created) as i64 - 116_444_736_000_000_000) / 10_000;
            details.started_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(unix_ms)
                .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            details.cpu_seconds = Some((ticks(kernel) + ticks(user)) as f64 / 10_000_000.0);
        }
        // SAFETY: PROCESS_MEMORY_COUNTERS is plain data with its size passed.
        let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let memory_size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if unsafe { K32GetProcessMemoryInfo(handle.0, &mut memory, memory_size) } != 0 {
            details.working_set_bytes = Some(memory.WorkingSetSize as u64);
        }
        if command_line {
            details.command_line = query_command_line(handle.0);
        }
        details
    }

    fn query_command_line(handle: HANDLE) -> Option<String> {
        let mut length = 0u32;
        // SAFETY: a zero-length query only reports the required size.
        unsafe {
            NtQueryInformationProcess(handle, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut length)
        };
        let needed = (length as usize).max(std::mem::size_of::<UNICODE_STRING>());
        if needed > 1 << 20 {
            return None;
        }
        // u64 storage keeps the UNICODE_STRING header aligned.
        let mut buffer = vec![0u64; needed.div_ceil(8)];
        // SAFETY: the buffer holds `needed` bytes; on success it starts with a
        // UNICODE_STRING whose Buffer points into this allocation.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut length,
            )
        };
        if status < 0 {
            return None;
        }
        // SAFETY: success guarantees the header; Length is in bytes.
        let text = unsafe {
            let header = &*buffer.as_ptr().cast::<UNICODE_STRING>();
            if header.Buffer.is_null() || header.Length == 0 {
                return Some(String::new());
            }
            let start = header.Buffer as usize;
            let base = buffer.as_ptr() as usize;
            let end = start.checked_add(usize::from(header.Length))?;
            if start < base || end > base + buffer.len() * 8 {
                return None;
            }
            std::slice::from_raw_parts(header.Buffer, usize::from(header.Length) / 2)
        };
        Some(String::from_utf16_lossy(text))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Details;
    use super::Entry;

    pub(super) fn snapshot() -> Result<Vec<Entry>, String> {
        Err("list_processes is only available on Windows".into())
    }

    pub(super) fn details(_pid: u32, _command_line: bool) -> Details {
        Details::default()
    }
}
