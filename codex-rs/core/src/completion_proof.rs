use crate::git_workspace::capture_workspace_evidence_identity;
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
#[cfg(not(all(feature = "completion-proof-test-store", debug_assertions)))]
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
#[cfg(all(feature = "completion-proof-test-store", debug_assertions))]
use codex_keyring_store::tests::MockKeyringStore;
use codex_protocol::ThreadId;
use hmac::Hmac;
use hmac::Mac;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
#[cfg(all(feature = "completion-proof-test-store", debug_assertions))]
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use uuid::Uuid;

const STATE_SCHEMA_VERSION: u32 = 1;
const AUTHENTICATED_STATE_SCHEMA_VERSION: u32 = 1;
const TRUST_ANCHOR_SCHEMA_VERSION: u32 = 1;
const TRUST_ANCHOR_SERVICE: &str = "Codex Completion Proof";
const TRUST_KEY_BYTES: usize = 32;
const ATTEMPT_SCHEMA_VERSION: u32 = 2;
const ATTEMPT_REPORT_TYPE: &str = "CompletionProofAttemptReportV2";
const FOCUSED_REPORT_TYPE: &str = "FocusedValidationAttemptReportV2";
const CONFIG_RELATIVE_PATH: &str = ".codex/validation/completion-proof.toml";
const INVENTORY_RELATIVE_PATH: &str = ".codex/validation/frozen-test-inventory-v1.json";
const REPLACEMENT_LEDGER_RELATIVE_PATH: &str = ".codex/validation/test-replacements-v1.json";
const KD4_POLICY_ID: &str = "kd4";
const KD4_FROZEN_INVENTORY_HASH: &str =
    "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152";
const CURRENT_USER_RELAXATION_SOURCE: &str = "current-user-message";
const RELAXATION_ALLOW_DIRECTIVE: &str = "completion-proof: allow completion without current proof";
const RELAXATION_REQUIRE_DIRECTIVE: &str = "completion-proof: require current proof";
const REPOSITORY_INSTRUCTION_FILENAMES: [&str; 2] = ["AGENTS.override.md", "AGENTS.md"];

const ENV_NONCE: &str = "CODEX_COMPLETION_PROOF_NONCE";
const ENV_REPORT: &str = "CODEX_COMPLETION_PROOF_REPORT";
const ENV_ATTEMPT_ID: &str = "CODEX_COMPLETION_PROOF_ATTEMPT_ID";
const ENV_PARENT_PID: &str = "CODEX_COMPLETION_PROOF_PARENT_PID";
const ENV_REPOSITORY: &str = "CODEX_COMPLETION_PROOF_REPOSITORY";
const ENV_START_FINGERPRINT: &str = "CODEX_COMPLETION_PROOF_START_FINGERPRINT";
const ENV_MUTATION_EPOCH: &str = "CODEX_COMPLETION_PROOF_MUTATION_EPOCH";
const ENV_POLICY_RUNNER_BUNDLE_SHA256: &str = "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256";
const ENV_RUNNER_ATTESTATION_ENDPOINT: &str = "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT";
const RUNNER_ATTESTATION_SCHEMA_VERSION: u32 = 1;
const MAX_RUNNER_ATTESTATION_BYTES: usize = 16 * 1024;
const RUNNER_ATTESTATION_WAIT: Duration = Duration::from_secs(2);
const PRIVATE_STATE_LOCK_WAIT: Duration = Duration::from_secs(10);
const PRIVATE_STATE_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const VALIDATION_INPUT_SNAPSHOT_SCHEMA_VERSION: u32 = 2;
const VALIDATION_INPUT_SNAPSHOT_ALGORITHM: &str = "content-and-path-set-v2";
const VALIDATION_INPUT_SNAPSHOT_ATTEMPTS: usize = 3;

#[derive(Clone, Copy)]
struct TrustedBundleMember {
    relative_path: &'static str,
    bytes: &'static [u8],
}

// These bytes are part of the runtime's trust boundary. Editing a live policy or evidence
// adapter cannot authorize itself; rebuilding KD4 is the explicit act that compiles new trusted
// bytes. Product and test sources are deliberately excluded because the workspace fingerprint
// binds them separately.
const KD4_TRUSTED_BUNDLE_MEMBERS: &[TrustedBundleMember] = &[
    TrustedBundleMember {
        relative_path: ".codex/validation/completion-proof.toml",
        bytes: include_bytes!("../../../.codex/validation/completion-proof.toml"),
    },
    TrustedBundleMember {
        relative_path: ".codex/validation/frozen-test-inventory-v1.json",
        bytes: include_bytes!("../../../.codex/validation/frozen-test-inventory-v1.json"),
    },
    TrustedBundleMember {
        relative_path: ".codex/validation/test-replacements-v1.json",
        bytes: include_bytes!("../../../.codex/validation/test-replacements-v1.json"),
    },
    TrustedBundleMember {
        relative_path: "justfile",
        bytes: include_bytes!("../../../justfile"),
    },
    TrustedBundleMember {
        relative_path: "scripts/just-shell.py",
        bytes: include_bytes!("../../../scripts/just-shell.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/rust_tool_env.py",
        bytes: include_bytes!("../../../scripts/rust_tool_env.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/completion_proof.py",
        bytes: include_bytes!("../../../scripts/completion_proof.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/bounded_process.py",
        bytes: include_bytes!("../../../scripts/bounded_process.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/completion_proof_unittest.py",
        bytes: include_bytes!("../../../scripts/completion_proof_unittest.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/root_maintenance.py",
        bytes: include_bytes!("../../../scripts/root_maintenance.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/source_map_check.py",
        bytes: include_bytes!("../../../scripts/source_map_check.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/asciicheck.py",
        bytes: include_bytes!("../../../scripts/asciicheck.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/readme_toc.py",
        bytes: include_bytes!("../../../scripts/readme_toc.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/source_owners.py",
        bytes: include_bytes!("../../../scripts/source_owners.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/generated_output_lock.py",
        bytes: include_bytes!("../../../scripts/generated_output_lock.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/config_schema_check.py",
        bytes: include_bytes!("../../../scripts/config_schema_check.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/app_server_schema_runtime_check.py",
        bytes: include_bytes!("../../../scripts/app_server_schema_runtime_check.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/completion_proof_pytest.py",
        bytes: include_bytes!("../../../scripts/completion_proof_pytest.py"),
    },
    TrustedBundleMember {
        relative_path: "tools/argument-comment-lint/native_test_runner.py",
        bytes: include_bytes!("../../../tools/argument-comment-lint/native_test_runner.py"),
    },
    TrustedBundleMember {
        relative_path: "codex-rs/windows-sandbox-rs/sandbox_smoketests.py",
        bytes: include_bytes!("../../windows-sandbox-rs/sandbox_smoketests.py"),
    },
    TrustedBundleMember {
        relative_path: "scripts/rust_test_runner.py",
        bytes: include_bytes!("../../../scripts/rust_test_runner.py"),
    },
    TrustedBundleMember {
        relative_path: "codex-rs/.config/kd4-rust-tests.toml",
        bytes: include_bytes!("../../.config/kd4-rust-tests.toml"),
    },
    TrustedBundleMember {
        relative_path: "codex-rs/config/scripts/generate-proto.ps1",
        bytes: include_bytes!("../../config/scripts/generate-proto.ps1"),
    },
];

#[derive(Clone, Debug)]
struct RunnerProcessAttestation {
    process_id: u32,
    executable_path: PathBuf,
    entrypoint_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerAttestationMessageV1 {
    schema_version: u32,
    attempt_id: String,
    nonce: String,
    process_id: u32,
    entrypoint_path: String,
}

#[derive(Debug)]
struct RunnerAttestationWaiter {
    endpoint: String,
    receiver: Option<oneshot::Receiver<Result<RunnerProcessAttestation, String>>>,
    task: tokio::task::JoinHandle<()>,
    cleanup_path: Option<PathBuf>,
}

impl RunnerAttestationWaiter {
    async fn receive(&mut self) -> Result<RunnerProcessAttestation, String> {
        let receiver = self
            .receiver
            .take()
            .ok_or_else(|| "the runner attestation was already consumed".to_string())?;
        match tokio::time::timeout(RUNNER_ATTESTATION_WAIT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the private runner attestation channel closed".to_string()),
            Err(_) => {
                Err("the trusted runner did not complete private process attestation".to_string())
            }
        }
    }
}

impl Drop for RunnerAttestationWaiter {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(path) = self.cleanup_path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn receive_runner_attestation<S>(
    stream: &mut S,
    expected_attempt_id: &str,
    expected_nonce: &str,
    peer_process_id: u32,
    peer_executable_path: PathBuf,
) -> Result<RunnerProcessAttestation, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut payload = Vec::new();
    loop {
        if payload.len() >= MAX_RUNNER_ATTESTATION_BYTES {
            return Err("the private runner attestation payload was too large".to_string());
        }
        let mut byte = [0_u8; 1];
        let read = stream
            .read(&mut byte)
            .await
            .map_err(|error| format!("could not read private runner attestation: {error}"))?;
        if read == 0 {
            return Err("the private runner attestation ended before its message".to_string());
        }
        if byte[0] == b'\n' {
            break;
        }
        payload.push(byte[0]);
    }
    let message = serde_json::from_slice::<RunnerAttestationMessageV1>(&payload)
        .map_err(|error| format!("the private runner attestation was invalid JSON: {error}"))?;
    if message.schema_version != RUNNER_ATTESTATION_SCHEMA_VERSION
        || message.attempt_id != expected_attempt_id
        || message.nonce != expected_nonce
        || message.process_id == 0
        || message.process_id != peer_process_id
    {
        return Err(
            "the private runner attestation did not match this exact spawned process and attempt"
                .to_string(),
        );
    }
    let entrypoint_path = canonical_existing_path(Path::new(&message.entrypoint_path)).await?;
    let executable_path = canonical_existing_path(&peer_executable_path).await?;
    stream
        .write_all(b"ok\n")
        .await
        .map_err(|error| format!("could not acknowledge private runner attestation: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("could not flush private runner attestation: {error}"))?;
    Ok(RunnerProcessAttestation {
        process_id: peer_process_id,
        executable_path,
        entrypoint_path,
    })
}

#[cfg(windows)]
async fn prepare_runner_attestation(
    attempt_id: &str,
    nonce: &str,
) -> Result<RunnerAttestationWaiter, String> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::prelude::OsStrExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let endpoint = format!(r"\\.\pipe\codex-completion-proof-{}", Uuid::new_v4());
    // The restricted-token runner can use a different local account. The unguessable pipe name
    // and invocation nonce authenticate the attempt; this DACL only makes the local transport
    // reachable. Remote clients remain rejected by Tokio's default pipe mode.
    let sddl = std::ffi::OsStr::new("D:(A;;GA;;;WD)")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(format!(
            "could not create private runner attestation security descriptor: {}",
            io::Error::last_os_error()
        ));
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let server_result = unsafe {
        options.create_with_security_attributes_raw(
            &endpoint,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    };
    unsafe {
        LocalFree(descriptor as HLOCAL);
    }
    let mut server = server_result
        .map_err(|error| format!("could not create private runner attestation pipe: {error}"))?;
    let expected_attempt_id = attempt_id.to_string();
    let expected_nonce = nonce.to_string();
    let (sender, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let result = async {
            server.connect().await.map_err(|error| {
                format!("the trusted runner did not connect for attestation: {error}")
            })?;
            let mut peer_process_id = 0_u32;
            let success = unsafe {
                GetNamedPipeClientProcessId(server.as_raw_handle(), &mut peer_process_id)
            };
            if success == 0 || peer_process_id == 0 {
                return Err(format!(
                    "could not authenticate the runner attestation process: {}",
                    io::Error::last_os_error()
                ));
            }
            let executable_path = windows_process_executable(peer_process_id)?;
            receive_runner_attestation(
                &mut server,
                &expected_attempt_id,
                &expected_nonce,
                peer_process_id,
                executable_path,
            )
            .await
        }
        .await;
        let _ = sender.send(result);
    });
    Ok(RunnerAttestationWaiter {
        endpoint,
        receiver: Some(receiver),
        task,
        cleanup_path: None,
    })
}

#[cfg(windows)]
fn windows_process_executable(process_id: u32) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(format!(
            "could not open attested runner process {process_id}: {}",
            io::Error::last_os_error()
        ));
    }
    let mut path = vec![0_u16; 32_768];
    let mut path_len = path.len() as u32;
    let success =
        unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut path_len) };
    unsafe {
        CloseHandle(process);
    }
    if success == 0 || path_len == 0 {
        return Err(format!(
            "could not resolve attested runner process {process_id}: {}",
            io::Error::last_os_error()
        ));
    }
    path.truncate(path_len as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&path)))
}

#[cfg(unix)]
async fn prepare_runner_attestation(
    attempt_id: &str,
    nonce: &str,
) -> Result<RunnerAttestationWaiter, String> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let socket_path =
        std::env::temp_dir().join(format!("codex-completion-proof-{}.sock", Uuid::new_v4()));
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("could not create private runner attestation socket: {error}"))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect private runner attestation socket: {error}"))?;
    let endpoint = socket_path.to_string_lossy().into_owned();
    let expected_attempt_id = attempt_id.to_string();
    let expected_nonce = nonce.to_string();
    let task_socket_path = socket_path.clone();
    let (sender, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let result = async {
            let (mut stream, _) = listener.accept().await.map_err(|error| {
                format!("the trusted runner did not connect for attestation: {error}")
            })?;
            let credentials = stream.peer_cred().map_err(|error| {
                format!("could not authenticate the runner attestation process: {error}")
            })?;
            let peer_process_id = credentials
                .pid()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .ok_or_else(|| {
                    "the operating system did not expose the runner attestation process id"
                        .to_string()
                })?;
            let executable_path = unix_process_executable(peer_process_id).await?;
            receive_runner_attestation(
                &mut stream,
                &expected_attempt_id,
                &expected_nonce,
                peer_process_id,
                executable_path,
            )
            .await
        }
        .await;
        let _ = std::fs::remove_file(task_socket_path);
        let _ = sender.send(result);
    });
    Ok(RunnerAttestationWaiter {
        endpoint,
        receiver: Some(receiver),
        task,
        cleanup_path: Some(socket_path),
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
async fn unix_process_executable(process_id: u32) -> Result<PathBuf, String> {
    tokio::fs::read_link(format!("/proc/{process_id}/exe"))
        .await
        .map_err(|error| format!("could not resolve attested runner process {process_id}: {error}"))
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
async fn unix_process_executable(process_id: u32) -> Result<PathBuf, String> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut path = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            process_id as libc::c_int,
            path.as_mut_ptr().cast(),
            path.len() as u32,
        )
    };
    if length <= 0 {
        return Err(format!(
            "could not resolve attested runner process {process_id}: {}",
            io::Error::last_os_error()
        ));
    }
    let path = CStr::from_bytes_until_nul(&path)
        .map_err(|_| "the attested runner executable path was not terminated".to_string())?;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))
))]
async fn unix_process_executable(process_id: u32) -> Result<PathBuf, String> {
    Err(format!(
        "this Unix host cannot authenticate runner process {process_id} executable identity"
    ))
}

#[cfg(not(any(unix, windows)))]
async fn prepare_runner_attestation(
    _attempt_id: &str,
    _nonce: &str,
) -> Result<RunnerAttestationWaiter, String> {
    Err("this host does not support private runner process attestation".to_string())
}

#[derive(Debug)]
pub(crate) struct CompletionProofReservation {
    attempt_id: String,
    report_write_root: PathBuf,
    repository_root: PathBuf,
    exact_command: String,
}

#[derive(Debug)]
pub(crate) struct CompletionProofAttempt {
    attempt_id: String,
    nonce: String,
    report_path: PathBuf,
    report_write_root: PathBuf,
    repository_root: PathBuf,
    exact_command: String,
    start_fingerprint: String,
    start_mutation_epoch: u64,
    parent_pid: u32,
    started_at_unix_ms: u64,
    expected_validation_ids: BTreeSet<String>,
    validation_evidence_contracts: BTreeMap<String, ValidationEvidenceContract>,
    validation_path_patterns: BTreeMap<String, ValidationInputContract>,
    expected_policy_id: String,
    policy_runner_bundle_sha256: String,
    trusted_runner_entrypoints: BTreeSet<TrustedRunnerEntrypoint>,
    expected_inventory_hash: String,
    expected_exceptions: BTreeSet<BaselineExceptionEvidence>,
    expected_overrides: BTreeSet<CompletionOverrideEvidence>,
    runner_attestation: RunnerAttestationWaiter,
}

#[derive(Debug)]
pub(crate) struct FocusedValidationAttempt {
    inner: CompletionProofAttempt,
    validation_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DocumentationValidationAttempt {
    exact_command: String,
    start_fingerprint: String,
    start_mutation_epoch: u64,
    policy_runner_bundle_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DocumentationValidationOutcome {
    ConfirmedPass,
    PreResultError { message: String },
}

impl DocumentationValidationOutcome {
    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::ConfirmedPass => {
                "Documentation validation accepted for the current workspace.".to_string()
            }
            Self::PreResultError { message } => format!(
                "The documentation validation produced no usable evidence: {message}. Correct the problem and rerun the exact documentation command."
            ),
        }
    }
}

impl CompletionProofAttempt {
    pub(crate) fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    /// The only private runtime directory that a canonical proof runner may
    /// write. Keeping each attempt in its own directory prevents one runner
    /// from modifying reports owned by another attempt.
    pub(crate) fn report_write_root(&self) -> &Path {
        &self.report_write_root
    }

    pub(crate) fn apply_private_environment(&self, env: &mut HashMap<String, String>) {
        env.retain(|name, _| {
            !name
                .to_ascii_uppercase()
                .starts_with("CODEX_COMPLETION_PROOF_")
        });
        env.insert(ENV_NONCE.to_string(), self.nonce.clone());
        env.insert(
            ENV_REPORT.to_string(),
            self.report_path.to_string_lossy().into_owned(),
        );
        env.insert(ENV_ATTEMPT_ID.to_string(), self.attempt_id.clone());
        env.insert(ENV_PARENT_PID.to_string(), self.parent_pid.to_string());
        env.insert(
            ENV_REPOSITORY.to_string(),
            self.repository_root.to_string_lossy().into_owned(),
        );
        env.insert(
            ENV_START_FINGERPRINT.to_string(),
            self.start_fingerprint.clone(),
        );
        env.insert(
            ENV_MUTATION_EPOCH.to_string(),
            self.start_mutation_epoch.to_string(),
        );
        env.insert(
            ENV_POLICY_RUNNER_BUNDLE_SHA256.to_string(),
            self.policy_runner_bundle_sha256.clone(),
        );
        env.insert(
            ENV_RUNNER_ATTESTATION_ENDPOINT.to_string(),
            self.runner_attestation.endpoint.clone(),
        );
    }

    fn persisted_pending(&self) -> PendingAttempt {
        PendingAttempt {
            attempt_id: self.attempt_id.clone(),
            nonce: self.nonce.clone(),
            report_path: self.report_path.clone(),
            repository_root: self.repository_root.clone(),
            exact_command: self.exact_command.clone(),
            start_fingerprint: self.start_fingerprint.clone(),
            start_mutation_epoch: self.start_mutation_epoch,
            parent_pid: self.parent_pid,
            started_at_unix_ms: self.started_at_unix_ms,
            expected_validation_ids: self.expected_validation_ids.clone(),
            validation_evidence_contracts: self.validation_evidence_contracts.clone(),
            validation_path_patterns: self.validation_path_patterns.clone(),
            expected_policy_id: self.expected_policy_id.clone(),
            policy_runner_bundle_sha256: self.policy_runner_bundle_sha256.clone(),
            trusted_runner_entrypoints: self.trusted_runner_entrypoints.clone(),
            expected_inventory_hash: self.expected_inventory_hash.clone(),
            expected_exceptions: self.expected_exceptions.clone(),
            expected_overrides: self.expected_overrides.clone(),
        }
    }
}

impl CompletionProofReservation {
    #[cfg(windows)]
    pub(crate) fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    #[cfg(windows)]
    pub(crate) fn exact_command(&self) -> &str {
        &self.exact_command
    }

    pub(crate) fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    pub(crate) fn report_write_root(&self) -> &Path {
        &self.report_write_root
    }

    #[cfg(windows)]
    pub(crate) fn apply_platform_preparation_environment(&self, env: &mut HashMap<String, String>) {
        env.retain(|name, _| {
            !name
                .to_ascii_uppercase()
                .starts_with("CODEX_COMPLETION_PROOF_")
        });
        env.insert(ENV_ATTEMPT_ID.to_string(), self.attempt_id.clone());
    }
}

impl FocusedValidationAttempt {
    pub(crate) fn apply_private_environment(&self, env: &mut HashMap<String, String>) {
        self.inner.apply_private_environment(env);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionProofAttemptOutcome {
    ConfirmedPass,
    ConfirmedValidationFailure { validation_ids: Vec<String> },
    PreResultError { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FocusedValidationOutcome {
    ConfirmedPass { validation_id: String },
    ConfirmedValidationFailure { validation_id: String },
    PreResultError { message: String },
}

impl FocusedValidationOutcome {
    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::ConfirmedPass { validation_id } => format!(
                "Focused validation {validation_id} actually ran and passed. This is implementation feedback only; it does not establish whole-repository certification."
            ),
            Self::ConfirmedValidationFailure { validation_id } => format!(
                "Focused validation {validation_id} actually ran and failed. A relevant corrective repository mutation is required before a later pass can count; no broader validation was started."
            ),
            Self::PreResultError { message } => format!(
                "The focused validation produced no usable pass or failure evidence: {message}. Correct the invocation or runner problem and rerun it; no broader validation was started."
            ),
        }
    }
}

impl CompletionProofAttemptOutcome {
    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::ConfirmedPass => {
                "Completion proof accepted for the current workspace.".to_string()
            }
            Self::ConfirmedValidationFailure { validation_ids } => format!(
                "Completion proof failed after these required validations actually ran: {}. A relevant corrective repository mutation is required before a later pass can count.",
                validation_ids.join(", ")
            ),
            Self::PreResultError { message } => format!(
                "Completion proof produced no usable proof because the canonical attempt ended before a complete confirmed result: {message}. Correct the invocation or runner problem and explicitly rerun the canonical command."
            ),
        }
    }
}

/// Move-owned completion-proof state for the model-visible unified-exec path.
///
/// The command handler owns an activated attempt until the process is durably
/// stored. At that point ownership moves to the process watcher, which finishes
/// the attempt only after output and repository observation are finalized.
/// Finishing runs in a detached task so cancelling a waiting tool future cannot
/// strand a private invocation nonce in the ledger.
#[derive(Clone)]
pub(crate) struct UnifiedExecCompletionProof {
    inner: Arc<UnifiedExecCompletionProofInner>,
}

impl std::fmt::Debug for UnifiedExecCompletionProof {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnifiedExecCompletionProof")
            .field("kind", &self.kind_name())
            .finish_non_exhaustive()
    }
}

struct UnifiedExecCompletionProofInner {
    session: Arc<crate::session::session::Session>,
    state: StdMutex<UnifiedExecCompletionProofState>,
    outcome: watch::Sender<Option<UnifiedExecCompletionProofOutcome>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnifiedExecCompletionProofOutcome {
    Canonical(CompletionProofAttemptOutcome),
    Focused(FocusedValidationOutcome),
    Documentation(DocumentationValidationOutcome),
}

impl UnifiedExecCompletionProofOutcome {
    pub(crate) fn accepted(&self) -> bool {
        matches!(
            self,
            Self::Canonical(CompletionProofAttemptOutcome::ConfirmedPass)
                | Self::Focused(FocusedValidationOutcome::ConfirmedPass { .. })
                | Self::Documentation(DocumentationValidationOutcome::ConfirmedPass)
        )
    }

    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::Canonical(outcome) => outcome.render_for_model(),
            Self::Focused(outcome) => outcome.render_for_model(),
            Self::Documentation(outcome) => outcome.render_for_model(),
        }
    }

    /// Adds the trusted-runner disposition only to the model-facing response.
    /// Raw output artifacts are finalized before this method is called, so the
    /// private attempt environment can never become durable command output.
    pub(crate) fn apply_to_exec_command_output(
        &self,
        output: &mut crate::tools::context::ExecCommandToolOutput,
    ) {
        let message = self.render_for_model();
        if !output.raw_output.is_empty() && !output.raw_output.ends_with(b"\n") {
            output.raw_output.push(b'\n');
        }
        output.raw_output.extend_from_slice(message.as_bytes());
        output.raw_output.push(b'\n');
        output.original_token_count = Some(codex_utils_string::approx_token_count(
            String::from_utf8_lossy(&output.raw_output).as_ref(),
        ));
        if !self.accepted() && output.exit_code.unwrap_or(0) == 0 {
            output.exit_code = Some(-1);
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UnifiedExecCompletionProofOwner {
    Caller,
    Watcher,
}

enum UnifiedExecCompletionProofState {
    Pending {
        owner: UnifiedExecCompletionProofOwner,
        attempt: UnifiedExecCompletionProofAttempt,
    },
    Finishing,
    Finished(UnifiedExecCompletionProofOutcome),
}

enum UnifiedExecCompletionProofAttempt {
    Canonical {
        attempt: CompletionProofAttempt,
        source_observation: crate::git_workspace::SourcePathChangeObservation,
        prior_observation: Option<PostProofMutationObservation>,
    },
    Focused(FocusedValidationAttempt),
    Documentation(DocumentationValidationAttempt),
}

impl UnifiedExecCompletionProof {
    pub(crate) fn canonical(
        session: Arc<crate::session::session::Session>,
        attempt: CompletionProofAttempt,
        source_observation: crate::git_workspace::SourcePathChangeObservation,
        prior_observation: Option<PostProofMutationObservation>,
    ) -> Self {
        Self::new(
            session,
            UnifiedExecCompletionProofAttempt::Canonical {
                attempt,
                source_observation,
                prior_observation,
            },
        )
    }

    pub(crate) fn focused(
        session: Arc<crate::session::session::Session>,
        attempt: FocusedValidationAttempt,
    ) -> Self {
        Self::new(session, UnifiedExecCompletionProofAttempt::Focused(attempt))
    }

    pub(crate) fn documentation(
        session: Arc<crate::session::session::Session>,
        attempt: DocumentationValidationAttempt,
    ) -> Self {
        Self::new(
            session,
            UnifiedExecCompletionProofAttempt::Documentation(attempt),
        )
    }

    fn new(
        session: Arc<crate::session::session::Session>,
        attempt: UnifiedExecCompletionProofAttempt,
    ) -> Self {
        let (outcome, _) = watch::channel(None);
        Self {
            inner: Arc::new(UnifiedExecCompletionProofInner {
                session,
                state: StdMutex::new(UnifiedExecCompletionProofState::Pending {
                    owner: UnifiedExecCompletionProofOwner::Caller,
                    attempt,
                }),
                outcome,
            }),
        }
    }

    fn kind_name(&self) -> &'static str {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            UnifiedExecCompletionProofState::Pending { attempt, .. } => match attempt {
                UnifiedExecCompletionProofAttempt::Canonical { .. } => "canonical",
                UnifiedExecCompletionProofAttempt::Focused(_) => "focused",
                UnifiedExecCompletionProofAttempt::Documentation(_) => "documentation",
            },
            UnifiedExecCompletionProofState::Finishing => "finishing",
            UnifiedExecCompletionProofState::Finished(outcome) => match outcome {
                UnifiedExecCompletionProofOutcome::Canonical(_) => "canonical",
                UnifiedExecCompletionProofOutcome::Focused(_) => "focused",
                UnifiedExecCompletionProofOutcome::Documentation(_) => "documentation",
            },
        }
    }

    pub(crate) fn is_canonical(&self) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(
            &*state,
            UnifiedExecCompletionProofState::Pending {
                attempt: UnifiedExecCompletionProofAttempt::Canonical { .. },
                ..
            } | UnifiedExecCompletionProofState::Finished(
                UnifiedExecCompletionProofOutcome::Canonical(_)
            )
        )
    }

    /// Removes every ambient completion-proof variable using Windows-style
    /// case-insensitive matching, then installs only the private values owned by
    /// this attempt. This must be called after shell/environment preparation.
    pub(crate) fn seal_private_environment(&self, env: &mut HashMap<String, String>) {
        env.retain(|name, _| {
            !name
                .to_ascii_uppercase()
                .starts_with("CODEX_COMPLETION_PROOF_")
        });
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let UnifiedExecCompletionProofState::Pending { attempt, .. } = &*state {
            match attempt {
                UnifiedExecCompletionProofAttempt::Canonical { attempt, .. } => {
                    attempt.apply_private_environment(env);
                }
                UnifiedExecCompletionProofAttempt::Focused(attempt) => {
                    attempt.apply_private_environment(env);
                }
                UnifiedExecCompletionProofAttempt::Documentation(_) => {}
            }
        }
    }

    pub(crate) fn transfer_to_watcher(&self) -> bool {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *state {
            UnifiedExecCompletionProofState::Pending { owner, .. }
                if *owner == UnifiedExecCompletionProofOwner::Caller =>
            {
                *owner = UnifiedExecCompletionProofOwner::Watcher;
                true
            }
            _ => false,
        }
    }

    pub(crate) async fn finish_if_caller_owned(
        &self,
        process_exit_code: Option<i32>,
    ) -> Option<UnifiedExecCompletionProofOutcome> {
        if !self.start_finish(UnifiedExecCompletionProofOwner::Caller, process_exit_code) {
            return self.current_outcome();
        }
        Some(self.await_outcome().await)
    }

    pub(crate) async fn finish_as_watcher(
        &self,
        process_exit_code: Option<i32>,
    ) -> UnifiedExecCompletionProofOutcome {
        self.start_finish(UnifiedExecCompletionProofOwner::Watcher, process_exit_code);
        self.await_outcome().await
    }

    pub(crate) async fn await_outcome(&self) -> UnifiedExecCompletionProofOutcome {
        if let Some(outcome) = self.current_outcome() {
            return outcome;
        }
        let mut receiver = self.inner.outcome.subscribe();
        loop {
            if let Some(outcome) = receiver.borrow().clone() {
                return outcome;
            }
            if receiver.changed().await.is_err() {
                return UnifiedExecCompletionProofOutcome::Canonical(
                    CompletionProofAttemptOutcome::PreResultError {
                        message: "the unified-exec proof owner ended before publishing an outcome"
                            .to_string(),
                    },
                );
            }
        }
    }

    fn current_outcome(&self) -> Option<UnifiedExecCompletionProofOutcome> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            UnifiedExecCompletionProofState::Finished(outcome) => Some(outcome.clone()),
            _ => None,
        }
    }

    fn start_finish(
        &self,
        expected_owner: UnifiedExecCompletionProofOwner,
        process_exit_code: Option<i32>,
    ) -> bool {
        let attempt = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let is_owner = matches!(
                &*state,
                UnifiedExecCompletionProofState::Pending { owner, .. }
                    if *owner == expected_owner
            );
            if !is_owner {
                return false;
            }
            match std::mem::replace(&mut *state, UnifiedExecCompletionProofState::Finishing) {
                UnifiedExecCompletionProofState::Pending { attempt, .. } => attempt,
                _ => unreachable!("completion-proof owner checked under the same lock"),
            }
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let outcome = finish_unified_exec_completion_proof_attempt(
                &inner.session,
                attempt,
                process_exit_code,
            )
            .await;
            {
                let mut state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *state = UnifiedExecCompletionProofState::Finished(outcome.clone());
            }
            inner.outcome.send_replace(Some(outcome));
        });
        true
    }
}

impl Drop for UnifiedExecCompletionProofInner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let attempt = match std::mem::replace(state, UnifiedExecCompletionProofState::Finishing) {
            UnifiedExecCompletionProofState::Pending { attempt, .. } => attempt,
            other => {
                *state = other;
                return;
            }
        };
        let session = Arc::clone(&self.session);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = finish_unified_exec_completion_proof_attempt(&session, attempt, None).await;
            });
        }
    }
}

async fn finish_unified_exec_completion_proof_attempt(
    session: &Arc<crate::session::session::Session>,
    attempt: UnifiedExecCompletionProofAttempt,
    process_exit_code: Option<i32>,
) -> UnifiedExecCompletionProofOutcome {
    match attempt {
        UnifiedExecCompletionProofAttempt::Canonical {
            attempt,
            source_observation,
            prior_observation,
        } => {
            let handoff = session
                .services
                .git_workspace
                .finish_source_path_change_observation_and_continue(&source_observation)
                .await;
            let observed_paths = handoff
                .as_ref()
                .and_then(crate::git_workspace::SourcePathChangeHandoff::classifiable_exact_paths);
            if let Some(prior_observation) = prior_observation {
                session
                    .services
                    .completion_proof
                    .finish_post_proof_mutation_observation(
                        prior_observation,
                        observed_paths.clone(),
                    )
                    .await;
            }
            let outcome = session
                .services
                .completion_proof
                .finish_canonical_attempt(attempt, process_exit_code, observed_paths)
                .await;
            if matches!(outcome, CompletionProofAttemptOutcome::ConfirmedPass)
                && let Some(handoff) = handoff
            {
                session
                    .services
                    .completion_proof
                    .bind_current_proof_source_observation(
                        handoff.continuation,
                        Arc::clone(&session.services.git_workspace),
                    )
                    .await;
            }
            UnifiedExecCompletionProofOutcome::Canonical(outcome)
        }
        UnifiedExecCompletionProofAttempt::Focused(attempt) => {
            UnifiedExecCompletionProofOutcome::Focused(
                session
                    .services
                    .completion_proof
                    .finish_focused_attempt(attempt, process_exit_code)
                    .await,
            )
        }
        UnifiedExecCompletionProofAttempt::Documentation(attempt) => {
            UnifiedExecCompletionProofOutcome::Documentation(
                session
                    .services
                    .completion_proof
                    .finish_documentation_validation(attempt, process_exit_code)
                    .await,
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionProofGateDecision {
    Accepted,
    Blocked { message: String },
}

#[derive(Clone, Debug)]
struct PendingAttempt {
    attempt_id: String,
    nonce: String,
    report_path: PathBuf,
    repository_root: PathBuf,
    exact_command: String,
    start_fingerprint: String,
    start_mutation_epoch: u64,
    parent_pid: u32,
    started_at_unix_ms: u64,
    expected_validation_ids: BTreeSet<String>,
    validation_evidence_contracts: BTreeMap<String, ValidationEvidenceContract>,
    validation_path_patterns: BTreeMap<String, ValidationInputContract>,
    expected_policy_id: String,
    policy_runner_bundle_sha256: String,
    trusted_runner_entrypoints: BTreeSet<TrustedRunnerEntrypoint>,
    expected_inventory_hash: String,
    expected_exceptions: BTreeSet<BaselineExceptionEvidence>,
    expected_overrides: BTreeSet<CompletionOverrideEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionProofArtifactV1 {
    schema_version: u32,
    artifact_type: String,
    policy_id: String,
    #[serde(default)]
    policy_runner_bundle_sha256: String,
    exact_command: String,
    repository_root: String,
    host_identity: HostIdentity,
    invocation_nonce: String,
    parent_pid: u32,
    observed_runner_parent_pid: u32,
    start_fingerprint: String,
    end_fingerprint: String,
    workspace_fingerprint: String,
    start_mutation_epoch: u64,
    end_mutation_epoch: u64,
    mutation_epoch: u64,
    inventory_hash: String,
    report_hash: String,
    attempt_report_hash: String,
    attempt_id: String,
    validations: Vec<ValidationAttemptReport>,
    child_processes: Vec<ChildProcessReport>,
    exceptions: Vec<BaselineExceptionEvidence>,
    overrides: Vec<CompletionOverrideEvidence>,
    registered_at_unix_ms: u64,
    artifact_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PoisonedValidation {
    validation_id: String,
    failed_at_mutation_epoch: u64,
    #[serde(default)]
    #[serde(alias = "observed_relevant_paths")]
    relevant_path_patterns: BTreeSet<String>,
    #[serde(default)]
    relevant_input_contract: Option<ValidationInputContract>,
    #[serde(default)]
    failure_input_snapshot: Option<ValidationInputSnapshotV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidationInputSnapshotV1 {
    schema_version: u32,
    algorithm: String,
    sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidationInputContract {
    content_paths: BTreeSet<String>,
    evidence_path_manifests: BTreeSet<String>,
    path_set_paths: BTreeSet<String>,
    schema_version: u32,
}

impl ValidationInputContract {
    fn digest(&self) -> Result<String, String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("could not encode validation input contract: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn all_declared_patterns(&self) -> BTreeSet<String> {
        self.content_paths
            .iter()
            .chain(&self.path_set_paths)
            .chain(&self.evidence_path_manifests)
            .cloned()
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum CompletionProofRelaxationDecision {
    AllowWithoutCurrentProof,
    RequireCurrentProof,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedCurrentUserRelaxation {
    session_lineage_id: String,
    decision: Option<CompletionProofRelaxationDecision>,
    evidence: Vec<CompletionOverrideEvidence>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AppliedCompletionProofRelaxation {
    authority: String,
    binding: String,
    evidence: Vec<CompletionOverrideEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistentCompletionProofState {
    schema_version: u32,
    repository_root: String,
    mutation_epoch: u64,
    last_observed_fingerprint: Option<String>,
    #[serde(default)]
    last_observed_head_identity: Option<String>,
    #[serde(default)]
    last_observed_head_identity_initialized: bool,
    #[serde(default)]
    last_observed_path_fingerprints: BTreeMap<String, String>,
    requires_non_documentation_proof: bool,
    requires_documentation_validation: bool,
    registered_proof: Option<CompletionProofArtifactV1>,
    #[serde(default)]
    poisoned_validations: BTreeMap<String, PoisonedValidation>,
    #[serde(default)]
    current_user_relaxation: Option<PersistedCurrentUserRelaxation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    current_user_relaxations_by_lineage: BTreeMap<String, PersistedCurrentUserRelaxation>,
    #[serde(default)]
    last_applied_relaxation: Option<AppliedCompletionProofRelaxation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedCompletionProofStateV1 {
    schema_version: u32,
    repository_key: String,
    key_id: String,
    revision: u64,
    state: PersistentCompletionProofState,
    authentication_tag: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionProofTrustAnchorV1 {
    // This anchor is the security boundary: it must stay outside repository and ordinary tool
    // write authority. The JSON state file is intentionally treated as attacker-writable.
    schema_version: u32,
    repository_key: String,
    key_id: String,
    authentication_key: String,
    committed_revision: u64,
    committed_envelope_hash: Option<String>,
}

#[derive(Clone)]
struct ProofStateSeal {
    key_id: String,
    authentication_key: [u8; TRUST_KEY_BYTES],
    revision: u64,
    envelope_hash: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionProofSessionRole {
    RootTerminalOwner,
    EvidenceContributor,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LiveIssuanceKey {
    repository_root: PathBuf,
    session_lineage_id: String,
}

impl LiveIssuanceKey {
    fn new(repository_root: &Path, session_lineage_id: String) -> Self {
        Self {
            repository_root: completion_proof_path_identity(repository_root),
            session_lineage_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveCompletionProofIssuance {
    artifact_hash: String,
    attempt_id: String,
    invocation_nonce: String,
    policy_id: String,
    policy_runner_bundle_sha256: String,
    inventory_hash: String,
    mutation_epoch: u64,
    state_revision: u64,
    state_envelope_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredRootRollout {
    issuance_key: LiveIssuanceKey,
    terminal_quiescence_root_thread_id: ThreadId,
}

/// Process-private authority and live proof state shared by every session admitted by one
/// `ThreadManager`. Nothing in this registry is serializable, so a runtime restart necessarily
/// loses issuance even when the authenticated diagnostic state remains on disk.
pub(crate) struct CompletionProofRuntimeRegistry {
    root_lineages: Mutex<HashSet<LiveIssuanceKey>>,
    root_rollout_lineages: Mutex<HashMap<PathBuf, RegisteredRootRollout>>,
    live_issuances: Mutex<HashMap<LiveIssuanceKey, LiveCompletionProofIssuance>>,
    post_proof_source_observations:
        Mutex<HashMap<LiveIssuanceKey, BoundPostProofSourceObservation>>,
    post_proof_source_transition: Mutex<()>,
    trust_store: Arc<dyn KeyringStore>,
}

#[cfg(all(feature = "completion-proof-test-store", debug_assertions))]
static COMPLETION_PROOF_TEST_TRUST_STORE: LazyLock<Arc<MockKeyringStore>> =
    LazyLock::new(|| Arc::new(MockKeyringStore::default()));

#[cfg(all(feature = "completion-proof-test-store", debug_assertions))]
fn completion_proof_runtime_trust_store() -> Arc<dyn KeyringStore> {
    Arc::clone(&COMPLETION_PROOF_TEST_TRUST_STORE) as Arc<dyn KeyringStore>
}

#[cfg(not(all(feature = "completion-proof-test-store", debug_assertions)))]
fn completion_proof_runtime_trust_store() -> Arc<dyn KeyringStore> {
    Arc::new(DefaultKeyringStore)
}

impl CompletionProofRuntimeRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            root_lineages: Mutex::new(HashSet::new()),
            root_rollout_lineages: Mutex::new(HashMap::new()),
            live_issuances: Mutex::new(HashMap::new()),
            post_proof_source_observations: Mutex::new(HashMap::new()),
            post_proof_source_transition: Mutex::new(()),
            trust_store: completion_proof_runtime_trust_store(),
        })
    }
}

/// Non-Serde capability supplied only by a trusted in-process admission boundary.
#[derive(Clone)]
pub(crate) struct CompletionProofSessionAuthority {
    role: CompletionProofSessionRole,
    runtime_registry: Arc<CompletionProofRuntimeRegistry>,
    repository_authority_eligible_at_admission: bool,
    repository_root: Option<PathBuf>,
    requested_existing_lineage_id: Option<String>,
    requested_terminal_quiescence_root_thread_id: Option<ThreadId>,
}

impl CompletionProofSessionAuthority {
    pub(crate) fn root_terminal_owner(
        runtime_registry: Arc<CompletionProofRuntimeRegistry>,
        cwd: &Path,
    ) -> Self {
        Self {
            role: CompletionProofSessionRole::RootTerminalOwner,
            runtime_registry,
            repository_authority_eligible_at_admission: codex_git_utils::get_git_repo_root(cwd)
                .is_some(),
            repository_root: Some(canonical_repository_root(cwd)),
            requested_existing_lineage_id: None,
            requested_terminal_quiescence_root_thread_id: None,
        }
    }

    /// Requests reuse of a lineage from persisted history, but authorizes it only when this
    /// manager's private registry issued that exact root lineage earlier in this process.
    pub(crate) fn root_terminal_owner_for_existing_lineage(
        runtime_registry: Arc<CompletionProofRuntimeRegistry>,
        cwd: &Path,
        requested_existing_lineage_id: Option<String>,
        terminal_quiescence_root_thread_id: ThreadId,
    ) -> Self {
        Self {
            role: CompletionProofSessionRole::RootTerminalOwner,
            runtime_registry,
            repository_authority_eligible_at_admission: codex_git_utils::get_git_repo_root(cwd)
                .is_some(),
            repository_root: Some(canonical_repository_root(cwd)),
            requested_existing_lineage_id: requested_existing_lineage_id
                .and_then(valid_exact_identity),
            requested_terminal_quiescence_root_thread_id: Some(terminal_quiescence_root_thread_id),
        }
    }

    pub(crate) fn evidence_contributor(
        runtime_registry: Arc<CompletionProofRuntimeRegistry>,
        cwd: &Path,
    ) -> Self {
        // Recognition is not ownership. A standalone worker must recognize the repository's
        // canonical command so the root-only check can reject it before any child is launched.
        Self::evidence_contributor_with_repository_admission(
            runtime_registry,
            codex_git_utils::get_git_repo_root(cwd).is_some(),
            canonical_repository_root(cwd),
        )
    }

    fn evidence_contributor_with_repository_admission(
        runtime_registry: Arc<CompletionProofRuntimeRegistry>,
        repository_authority_eligible_at_admission: bool,
        repository_root: PathBuf,
    ) -> Self {
        Self {
            role: CompletionProofSessionRole::EvidenceContributor,
            runtime_registry,
            repository_authority_eligible_at_admission,
            repository_root: Some(repository_root),
            requested_existing_lineage_id: None,
            requested_terminal_quiescence_root_thread_id: None,
        }
    }

    pub(crate) fn is_terminal_owner(&self) -> bool {
        self.role == CompletionProofSessionRole::RootTerminalOwner
    }

    pub(crate) fn terminal_quiescence_root_thread_id(
        &self,
        fallback_thread_id: ThreadId,
    ) -> Option<ThreadId> {
        self.is_terminal_owner().then_some(
            self.requested_terminal_quiescence_root_thread_id
                .unwrap_or(fallback_thread_id),
        )
    }

    /// Binds the non-Serde admission capability to one process-private root lineage. Serialized
    /// session metadata can nominate a lineage, but it cannot create or recover membership.
    pub(crate) async fn bind_session_lineage_id(&self, fallback_lineage_id: String) -> String {
        if !self.is_terminal_owner() {
            return fallback_lineage_id;
        }
        let Some(repository_root) = self.repository_root.as_ref() else {
            return fallback_lineage_id;
        };
        let fallback_lineage_id =
            valid_exact_identity(fallback_lineage_id).unwrap_or_else(|| Uuid::now_v7().to_string());
        let mut root_lineages = self.runtime_registry.root_lineages.lock().await;
        let requested = self
            .requested_existing_lineage_id
            .as_ref()
            .map(|session_lineage_id| {
                LiveIssuanceKey::new(repository_root, session_lineage_id.clone())
            })
            .filter(|key| root_lineages.contains(key));
        let fallback = LiveIssuanceKey::new(repository_root, fallback_lineage_id);
        let resolved = match requested {
            Some(requested) => requested,
            // A fresh admission may carry a serialized lineage identifier from an unregistered
            // copy of a rollout. Never let that caller-controlled copy collide with an existing
            // process-private root lineage and inherit its live issuance.
            None if root_lineages.contains(&fallback) => {
                LiveIssuanceKey::new(repository_root, Uuid::now_v7().to_string())
            }
            None => fallback,
        };
        root_lineages.insert(resolved.clone());
        resolved.session_lineage_id
    }

    /// Binds the exact stored rollout admitted by this manager to its process-private root
    /// lineage. A copied rollout at another path therefore cannot replay serialized metadata to
    /// inherit a live issuance, and a new manager starts with no reusable rollout bindings.
    pub(crate) async fn register_root_rollout(
        &self,
        rollout_path: &Path,
        session_lineage_id: &str,
        terminal_quiescence_root_thread_id: ThreadId,
    ) {
        if !self.is_terminal_owner() {
            return;
        }
        let (Some(repository_root), Some(rollout_path), Some(session_lineage_id)) = (
            self.repository_root.as_ref(),
            canonical_rollout_path(rollout_path),
            valid_exact_identity(session_lineage_id.to_string()),
        ) else {
            return;
        };
        let key = LiveIssuanceKey::new(repository_root, session_lineage_id);
        if self
            .runtime_registry
            .root_lineages
            .lock()
            .await
            .contains(&key)
        {
            self.runtime_registry
                .root_rollout_lineages
                .lock()
                .await
                .insert(
                    rollout_path,
                    RegisteredRootRollout {
                        issuance_key: key,
                        terminal_quiescence_root_thread_id,
                    },
                );
        }
    }
}

impl CompletionProofRuntimeRegistry {
    pub(crate) async fn registered_root_lineage_for_rollout(
        &self,
        cwd: &Path,
        rollout_path: &Path,
    ) -> Option<(String, ThreadId)> {
        let rollout_path = canonical_rollout_path(rollout_path)?;
        let repository_root = completion_proof_path_identity(&canonical_repository_root(cwd));
        self.root_rollout_lineages
            .lock()
            .await
            .get(&rollout_path)
            .filter(|registration| registration.issuance_key.repository_root == repository_root)
            .map(|registration| {
                (
                    registration.issuance_key.session_lineage_id.clone(),
                    registration.terminal_quiescence_root_thread_id,
                )
            })
    }
}

fn canonical_rollout_path(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = dunce::canonicalize(path) {
        return Some(completion_proof_path_identity(&path));
    }

    for ancestor in path.ancestors().skip(1) {
        let canonicalize_target = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        let Ok(mut canonical_ancestor) = dunce::canonicalize(canonicalize_target) else {
            continue;
        };
        let missing_suffix = path.strip_prefix(ancestor).ok()?;
        canonical_ancestor.push(missing_suffix);
        return Some(completion_proof_path_identity(&canonical_ancestor));
    }

    None
}

pub(crate) fn same_canonical_completion_proof_path(left: &Path, right: &Path) -> bool {
    matches!(
        (canonical_rollout_path(left), canonical_rollout_path(right)),
        (Some(left), Some(right)) if left == right
    )
}

enum ProofStateIntegrity {
    Trusted(ProofStateSeal),
    Blocked(String),
}

#[derive(Clone, Debug)]
enum CompletionProofRelaxationResolution {
    None,
    Clear {
        decision: CompletionProofRelaxationDecision,
        evidence: Vec<CompletionOverrideEvidence>,
    },
    Ambiguous {
        evidence: Vec<CompletionOverrideEvidence>,
    },
    Invalid {
        message: String,
    },
}

#[derive(Clone, Debug)]
struct RepositoryRelaxationSnapshot {
    head_oid: Option<String>,
    instruction_paths: BTreeSet<String>,
    resolution: CompletionProofRelaxationResolution,
}

impl Default for RepositoryRelaxationSnapshot {
    fn default() -> Self {
        Self {
            head_oid: None,
            instruction_paths: BTreeSet::new(),
            resolution: CompletionProofRelaxationResolution::None,
        }
    }
}

#[derive(Clone, Debug)]
enum EffectiveCompletionProofRelaxation {
    Allowed(AppliedCompletionProofRelaxation),
    NotAllowed { reason: Option<String> },
}

impl PersistentCompletionProofState {
    fn new(repository_root: &Path) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            repository_root: repository_root.to_string_lossy().into_owned(),
            mutation_epoch: 0,
            last_observed_fingerprint: None,
            last_observed_head_identity: None,
            last_observed_head_identity_initialized: false,
            last_observed_path_fingerprints: BTreeMap::new(),
            requires_non_documentation_proof: false,
            requires_documentation_validation: false,
            registered_proof: None,
            poisoned_validations: BTreeMap::new(),
            current_user_relaxation: None,
            current_user_relaxations_by_lineage: BTreeMap::new(),
            last_applied_relaxation: None,
        }
    }

    fn fail_closed(repository_root: &Path) -> Self {
        let mut state = Self::new(repository_root);
        state.requires_non_documentation_proof = true;
        state
    }
}

struct CompletionProofState {
    persistent: PersistentCompletionProofState,
    pending_attempts: HashMap<String, PendingAttempt>,
    integrity: ProofStateIntegrity,
}

#[derive(Clone, Debug)]
struct CompletionProofPersistence {
    state_path: PathBuf,
    lock_path: PathBuf,
    attempts_dir: PathBuf,
    repository_key: String,
    trust_anchor_account: String,
    trust_store: Arc<dyn KeyringStore>,
}

pub(crate) struct CompletionProofLedger {
    repository_root: PathBuf,
    requires_compiled_kd4_authority: bool,
    repository_authority_eligible_at_lineage_start: bool,
    session_role: CompletionProofSessionRole,
    runtime_registry: Arc<CompletionProofRuntimeRegistry>,
    session_lineage_id: Option<String>,
    repository_relaxation: RepositoryRelaxationSnapshot,
    operation: Mutex<()>,
    state: Mutex<CompletionProofState>,
    persistence: CompletionProofPersistence,
}

#[derive(Clone, Debug)]
pub(crate) struct PostProofMutationObservation {
    repository_root: PathBuf,
    start_mutation_epoch: u64,
    start_fingerprint: String,
    proof_artifact_hash: String,
}

#[derive(Clone)]
struct BoundPostProofSourceObservation {
    proof_observation: PostProofMutationObservation,
    source_observation: crate::git_workspace::SourcePathChangeObservation,
    source_workspace: Arc<crate::git_workspace::GitWorkspaceCache>,
}

impl CompletionProofLedger {
    pub(crate) fn is_terminal_owner(&self) -> bool {
        self.session_role == CompletionProofSessionRole::RootTerminalOwner
    }

    pub(crate) fn descendant_authority(&self) -> CompletionProofSessionAuthority {
        // Preserve the parent's admitted repository snapshot without granting its terminal role.
        CompletionProofSessionAuthority::evidence_contributor_with_repository_admission(
            Arc::clone(&self.runtime_registry),
            self.repository_authority_eligible_at_lineage_start,
            self.repository_root.clone(),
        )
    }

    fn live_issuance_key(&self) -> Option<LiveIssuanceKey> {
        (self.session_role == CompletionProofSessionRole::RootTerminalOwner)
            .then(|| self.session_lineage_id.as_ref())
            .flatten()
            .map(|session_lineage_id| {
                LiveIssuanceKey::new(&self.repository_root, session_lineage_id.clone())
            })
    }

    fn live_issuance_for(
        artifact: &CompletionProofArtifactV1,
        seal: &ProofStateSeal,
    ) -> Option<LiveCompletionProofIssuance> {
        let state_envelope_hash = seal.envelope_hash.clone()?;
        Some(LiveCompletionProofIssuance {
            artifact_hash: artifact.artifact_hash.clone(),
            attempt_id: artifact.attempt_id.clone(),
            invocation_nonce: artifact.invocation_nonce.clone(),
            policy_id: artifact.policy_id.clone(),
            policy_runner_bundle_sha256: artifact.policy_runner_bundle_sha256.clone(),
            inventory_hash: artifact.inventory_hash.clone(),
            mutation_epoch: artifact.mutation_epoch,
            state_revision: seal.revision,
            state_envelope_hash,
        })
    }

    fn live_issuance_matches_artifact(
        issuance: &LiveCompletionProofIssuance,
        artifact: &CompletionProofArtifactV1,
    ) -> bool {
        issuance.artifact_hash == artifact.artifact_hash
            && issuance.attempt_id == artifact.attempt_id
            && issuance.invocation_nonce == artifact.invocation_nonce
            && issuance.policy_id == artifact.policy_id
            && issuance.policy_runner_bundle_sha256 == artifact.policy_runner_bundle_sha256
            && issuance.inventory_hash == artifact.inventory_hash
            && issuance.mutation_epoch == artifact.mutation_epoch
    }

    async fn has_exact_live_issuance(&self, artifact: &CompletionProofArtifactV1) -> bool {
        let Some(key) = self.live_issuance_key() else {
            return false;
        };
        self.runtime_registry
            .live_issuances
            .lock()
            .await
            .get(&key)
            .is_some_and(|issuance| Self::live_issuance_matches_artifact(issuance, artifact))
    }

    async fn revoke_live_issuance(&self) {
        let Some(key) = self.live_issuance_key() else {
            return;
        };
        self.runtime_registry
            .live_issuances
            .lock()
            .await
            .remove(&key);
        self.runtime_registry
            .post_proof_source_observations
            .lock()
            .await
            .remove(&key);
    }

    /// Commits the authenticated candidate, reads it back through the authenticated loader, and
    /// only then installs the process-private issuance. Merely writing the state file is never
    /// sufficient to authorize terminal publication.
    async fn persist_and_install_live_issuance(&self) -> Result<(), String> {
        self.persist().await?;
        let Some(key) = self.live_issuance_key() else {
            return Err(
                "only an explicitly admitted root terminal owner may receive live completion-proof issuance"
                    .to_string(),
            );
        };
        let (readback, readback_seal) =
            load_authenticated_state(&self.persistence, &self.repository_root).await?;
        let readback_artifact = readback.registered_proof.as_ref().ok_or_else(|| {
            "the authenticated completion-proof readback omitted the committed artifact".to_string()
        })?;
        if completion_artifact_hash(readback_artifact).ok().as_deref()
            != Some(readback_artifact.artifact_hash.as_str())
        {
            self.revoke_live_issuance().await;
            return Err(
                "the authenticated completion-proof readback artifact hash was not exact"
                    .to_string(),
            );
        }
        let in_memory_artifact_hash = self
            .state
            .lock()
            .await
            .persistent
            .registered_proof
            .as_ref()
            .map(|artifact| artifact.artifact_hash.clone());
        if in_memory_artifact_hash.as_deref() != Some(readback_artifact.artifact_hash.as_str()) {
            self.revoke_live_issuance().await;
            return Err(
                "the authenticated completion-proof readback did not match the in-memory candidate"
                    .to_string(),
            );
        }
        let issuance = Self::live_issuance_for(readback_artifact, &readback_seal).ok_or_else(|| {
            "the authenticated completion-proof readback omitted its committed envelope identity"
                .to_string()
        })?;
        self.runtime_registry
            .live_issuances
            .lock()
            .await
            .insert(key, issuance);
        Ok(())
    }

    /// Holds the repository operation gate for this ledger's frozen root.
    /// Terminal publication uses this to keep the final proof check and the
    /// release of buffered output in the same serialized workspace interval.
    pub(crate) async fn acquire_workspace_operation(&self) -> tokio::sync::OwnedMutexGuard<()> {
        crate::workspace_operation_gate::acquire_workspace_operation(&self.repository_root).await
    }

    pub(crate) fn repository_root_for_cwd(&self, cwd: &Path) -> Option<PathBuf> {
        same_completion_proof_path(&canonical_repository_root(cwd), &self.repository_root)
            .then(|| self.repository_root.clone())
    }

    async fn authority_for_command_recognition(&self) -> Result<CompletionProofAuthority, String> {
        if self.requires_compiled_kd4_authority {
            compiled_kd4_authority()
        } else if !self.repository_authority_eligible_at_lineage_start {
            Err(
                "repository completion-proof authority is unavailable because this root-session lineage did not start in a Git repository"
                    .to_string(),
            )
        } else {
            load_completion_proof_authority(&self.repository_root, false).await
        }
    }

    async fn verified_authority(&self) -> Result<CompletionProofAuthority, String> {
        if !self.requires_compiled_kd4_authority
            && !self.repository_authority_eligible_at_lineage_start
        {
            return Err(
                "repository completion-proof authority is unavailable because this root-session lineage did not start in a Git repository"
                    .to_string(),
            );
        }
        load_completion_proof_authority(&self.repository_root, self.requires_compiled_kd4_authority)
            .await
    }

    async fn verify_pending_authority(
        &self,
        pending: &PendingAttempt,
    ) -> Result<CompletionProofAuthority, String> {
        let authority = self.verified_authority().await?;
        if authority.policy_runner_bundle_sha256 != pending.policy_runner_bundle_sha256
            || authority.trusted_runner_entrypoints != pending.trusted_runner_entrypoints
            || authority.config.policy_id != pending.expected_policy_id
            || authority.config.frozen_inventory_hash != pending.expected_inventory_hash
        {
            return Err(
                "the trusted completion-proof policy or runner bundle changed during validation"
                    .to_string(),
            );
        }
        Ok(authority)
    }

    #[cfg(test)]
    pub(crate) async fn load_or_new(codex_home: PathBuf, cwd: &Path, terminal_owner: bool) -> Self {
        let registry = CompletionProofRuntimeRegistry::new();
        let trust_store = Arc::clone(&registry.trust_store);
        let authority = if terminal_owner {
            CompletionProofSessionAuthority::root_terminal_owner(registry, cwd)
        } else {
            CompletionProofSessionAuthority::evidence_contributor(registry, cwd)
        };
        Self::load_or_new_with_trust_store(codex_home, cwd, authority, trust_store).await
    }

    /// Loads proof state for one root-session lineage. The lineage identity is kept private and
    /// binds current-user relaxation evidence across compaction, resume, and descendant forks
    /// without letting an unrelated session inherit it.
    pub(crate) async fn load_or_new_for_session(
        codex_home: PathBuf,
        cwd: &Path,
        session_authority: CompletionProofSessionAuthority,
        session_lineage_id: String,
    ) -> Self {
        let trust_store = Arc::clone(&session_authority.runtime_registry.trust_store);
        Self::load_or_new_with_context_and_trust_store(
            codex_home,
            cwd,
            session_authority,
            valid_exact_identity(session_lineage_id),
            trust_store,
        )
        .await
    }

    #[cfg(test)]
    async fn load_or_new_with_trust_store(
        codex_home: PathBuf,
        cwd: &Path,
        session_authority: CompletionProofSessionAuthority,
        trust_store: Arc<dyn KeyringStore>,
    ) -> Self {
        Self::load_or_new_with_context_and_trust_store(
            codex_home,
            cwd,
            session_authority,
            None,
            trust_store,
        )
        .await
    }

    async fn load_or_new_with_context_and_trust_store(
        codex_home: PathBuf,
        cwd: &Path,
        session_authority: CompletionProofSessionAuthority,
        session_lineage_id: Option<String>,
        trust_store: Arc<dyn KeyringStore>,
    ) -> Self {
        let repository_root = canonical_repository_root(cwd);
        let requires_compiled_kd4_authority =
            repository_requires_compiled_kd4_authority(&repository_root).await;
        // A non-Serde authority is bound to the exact repository admitted by its constructor; it
        // cannot be carried to a different working tree to obtain recognition there.
        let repository_authority_eligible_at_lineage_start = session_authority
            .repository_root
            .as_ref()
            .is_some_and(|admitted_root| {
                same_completion_proof_path(admitted_root, &repository_root)
            })
            && session_authority.repository_authority_eligible_at_admission;
        let repository_relaxation = if repository_authority_eligible_at_lineage_start {
            capture_repository_relaxation(&repository_root).await
        } else {
            RepositoryRelaxationSnapshot::default()
        };
        let repository_key = repository_state_key(&repository_root);
        let persistence = CompletionProofPersistence {
            state_path: codex_home
                .join("completion-proof")
                .join("repositories")
                .join(format!("{repository_key}.json")),
            lock_path: private_state_lock_path(&codex_home, &repository_root),
            attempts_dir: codex_home
                .join("completion-proof")
                .join("attempts")
                .join(&repository_key),
            repository_key: repository_key.clone(),
            trust_anchor_account: proof_state_trust_anchor_account(&codex_home, &repository_root),
            trust_store,
        };
        let state_file_lock = acquire_private_state_lock(persistence.lock_path.clone()).await;
        let (mut persistent, integrity) = match state_file_lock.as_ref() {
            Ok(_) => {
                match load_or_initialize_authenticated_state(&persistence, &repository_root).await {
                    Ok(loaded) => loaded,
                    Err(error) => (
                        PersistentCompletionProofState::fail_closed(&repository_root),
                        ProofStateIntegrity::Blocked(error),
                    ),
                }
            }
            Err(error) => (
                PersistentCompletionProofState::fail_closed(&repository_root),
                ProofStateIntegrity::Blocked(format!(
                    "the private completion-proof state lock was unavailable while loading: {error}"
                )),
            ),
        };
        if matches!(integrity, ProofStateIntegrity::Trusted(_)) {
            let current_observation = workspace_observation(&repository_root).await;
            reconcile_external_workspace_change(
                &repository_root,
                &mut persistent,
                current_observation,
            )
            .await;
        }
        let ledger = Self {
            repository_root,
            requires_compiled_kd4_authority,
            repository_authority_eligible_at_lineage_start,
            session_role: session_authority.role,
            runtime_registry: session_authority.runtime_registry,
            session_lineage_id,
            repository_relaxation,
            operation: Mutex::new(()),
            state: Mutex::new(CompletionProofState {
                persistent,
                pending_attempts: HashMap::new(),
                integrity,
            }),
            persistence,
        };
        if matches!(
            ledger.state.lock().await.integrity,
            ProofStateIntegrity::Trusted(_)
        ) {
            let _ = ledger.persist().await;
        }
        drop(state_file_lock);
        ledger
    }

    /// Records only an explicit completion-proof directive from a real current-user message.
    /// Callers must supply a trusted message identity, never text synthesized by the model or a
    /// tool. Unrelated prose is ignored and conflicting directives are persisted as ambiguous so
    /// that an earlier allowance cannot remain effective.
    pub(crate) async fn observe_current_user_completion_proof_instruction(
        &self,
        text: &str,
        message_identity: &str,
    ) -> Result<bool, String> {
        let Some(session_lineage_id) = self.session_lineage_id.as_ref() else {
            return Err(
                "current-user completion-proof instructions require a bound root-session lineage"
                    .to_string(),
            );
        };
        if !self.is_terminal_owner() {
            return Err(
                "only the root Codex session may record current-user completion-proof instructions"
                    .to_string(),
            );
        }
        let Some(message_identity) = valid_exact_identity(message_identity.to_string()) else {
            return Err(
                "current-user completion-proof instruction provenance was not exact and nonempty"
                    .to_string(),
            );
        };
        let provenance =
            format!("session:{session_lineage_id}/current-user-message:{message_identity}");
        let resolution =
            parse_relaxation_directives(text, CURRENT_USER_RELAXATION_SOURCE, &provenance);
        let persisted = match resolution {
            CompletionProofRelaxationResolution::None => return Ok(false),
            CompletionProofRelaxationResolution::Clear { decision, evidence } => {
                PersistedCurrentUserRelaxation {
                    session_lineage_id: session_lineage_id.clone(),
                    decision: Some(decision),
                    evidence,
                }
            }
            CompletionProofRelaxationResolution::Ambiguous { evidence } => {
                PersistedCurrentUserRelaxation {
                    session_lineage_id: session_lineage_id.clone(),
                    decision: None,
                    evidence,
                }
            }
            CompletionProofRelaxationResolution::Invalid { message } => return Err(message),
        };

        let _operation = self.operation.lock().await;
        let _state_file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .map_err(|error| {
                format!(
                    "the private completion-proof state lock was unavailable while recording current-user provenance: {error}"
                )
            })?;
        self.refresh_persistent_state_from_disk().await?;
        {
            let mut state = self.state.lock().await;
            state
                .persistent
                .current_user_relaxations_by_lineage
                .insert(session_lineage_id.clone(), persisted);
        }
        self.persist().await?;
        Ok(true)
    }

    pub(crate) async fn reserve_canonical_attempt(
        &self,
        displayed_command: &str,
        cwd: &Path,
    ) -> Result<Option<CompletionProofReservation>, String> {
        if !same_completion_proof_path(&canonical_repository_root(cwd), &self.repository_root) {
            return Ok(None);
        }
        let recognition_authority = match self.authority_for_command_recognition().await {
            Ok(authority) => authority,
            Err(_) => return Ok(None),
        };
        if displayed_command != recognition_authority.config.canonical_command {
            return Ok(None);
        }
        let authority = self.verified_authority().await?;
        if displayed_command != authority.config.canonical_command {
            return Err(
                "the trusted canonical command changed while certification was being prepared"
                    .to_string(),
            );
        }
        let config = authority.config;
        if !self.is_terminal_owner() {
            return Err(
                "only the root Codex session may own and register whole-repository certification"
                    .to_string(),
            );
        }
        let attempt_id = Uuid::now_v7().to_string();
        let report_write_root = self.persistence.attempts_dir.join(&attempt_id);
        if tokio::fs::try_exists(&report_write_root)
            .await
            .unwrap_or(true)
        {
            return Err("the private completion-proof report path was not fresh".to_string());
        }
        tokio::fs::create_dir_all(&report_write_root)
            .await
            .map_err(|error| {
                format!("the private completion-proof report directory was unavailable: {error}")
            })?;

        Ok(Some(CompletionProofReservation {
            attempt_id,
            report_write_root,
            repository_root: self.repository_root.clone(),
            exact_command: config.canonical_command,
        }))
    }

    /// Activates a reserved canonical launch only after any trusted platform
    /// setup has completed and the caller has handed its source watcher to the
    /// measured interval without a gap.
    pub(crate) async fn activate_canonical_attempt(
        &self,
        reservation: &CompletionProofReservation,
    ) -> Result<CompletionProofAttempt, String> {
        if reservation.repository_root != self.repository_root {
            return Err(
                "the canonical reservation does not belong to this repository ledger".to_string(),
            );
        }
        let authority = self.verified_authority().await?;
        if authority.config.canonical_command != reservation.exact_command {
            return Err(
                "the trusted canonical command changed during launch preparation".to_string(),
            );
        }
        let config = authority.config;
        let expected_inventory_hash = load_inventory_hash(&self.repository_root, &config).await?;
        let (expected_exceptions, expected_overrides) =
            load_expected_provenance(&self.repository_root, &config, &expected_inventory_hash)
                .await?;

        let _operation = self.operation.lock().await;
        let _state_file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .map_err(|error| {
                format!(
                    "the private completion-proof state lock was unavailable while activating certification: {error}"
                )
            })?;
        self.refresh_persistent_state_from_disk().await?;
        let Some(start_observation) = workspace_observation(&self.repository_root).await else {
            return Err(
                "the trusted runner could not capture the repository workspace fingerprint"
                    .to_string(),
            );
        };
        let start_fingerprint = start_observation.fingerprint.clone();
        let start_mutation_epoch = {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                Some(start_observation),
            )
            .await;
            state.persistent.mutation_epoch
        };
        self.persist().await?;

        let nonce = Uuid::new_v4().as_simple().to_string();
        let report_path = reservation.report_write_root.join("report.json");
        let runner_attestation =
            prepare_runner_attestation(&reservation.attempt_id, &nonce).await?;
        let pending = CompletionProofAttempt {
            attempt_id: reservation.attempt_id.clone(),
            nonce,
            report_path,
            report_write_root: reservation.report_write_root.clone(),
            repository_root: self.repository_root.clone(),
            exact_command: config.canonical_command,
            start_fingerprint,
            start_mutation_epoch,
            parent_pid: std::process::id(),
            started_at_unix_ms: unix_time_ms(),
            expected_validation_ids: config.validation_ids,
            validation_evidence_contracts: config.validation_evidence_contracts,
            validation_path_patterns: config.validation_path_patterns,
            expected_policy_id: config.policy_id,
            policy_runner_bundle_sha256: authority.policy_runner_bundle_sha256,
            trusted_runner_entrypoints: authority.trusted_runner_entrypoints,
            expected_inventory_hash,
            expected_exceptions,
            expected_overrides,
            runner_attestation,
        };
        self.state
            .lock()
            .await
            .pending_attempts
            .insert(pending.attempt_id.clone(), pending.persisted_pending());
        Ok(pending)
    }

    pub(crate) async fn discard_canonical_reservation(
        &self,
        reservation: &CompletionProofReservation,
    ) {
        let _ = tokio::fs::remove_dir_all(&reservation.report_write_root).await;
    }

    pub(crate) async fn prepare_focused_attempt(
        &self,
        displayed_command: &str,
        cwd: &Path,
    ) -> Result<Option<FocusedValidationAttempt>, String> {
        if !same_completion_proof_path(&canonical_repository_root(cwd), &self.repository_root) {
            return Ok(None);
        }
        let recognition_authority = match self.authority_for_command_recognition().await {
            Ok(authority) => authority,
            Err(_) => return Ok(None),
        };
        let recognition_matches = recognition_authority
            .config
            .validation_ids
            .iter()
            .filter(|validation_id| {
                recognition_authority
                    .config
                    .focused_command
                    .replace("{validation_id}", validation_id)
                    == displayed_command
            })
            .count();
        if recognition_matches == 0 {
            return Ok(None);
        }
        let authority = self.verified_authority().await?;
        let config = authority.config;
        let matching_validation_ids = config
            .validation_ids
            .iter()
            .filter(|validation_id| {
                config
                    .focused_command
                    .replace("{validation_id}", validation_id)
                    == displayed_command
            })
            .cloned()
            .collect::<Vec<_>>();
        let validation_id = match matching_validation_ids.as_slice() {
            [validation_id] => validation_id.clone(),
            [] => {
                return Err(
                    "the trusted focused command changed while validation was being prepared"
                        .to_string(),
                );
            }
            _ => {
                return Err(
                    "the configured focused command matched more than one validation identity"
                        .to_string(),
                );
            }
        };

        let _operation = self.operation.lock().await;
        let _state_file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .map_err(|error| {
                format!("the private completion-proof state lock was unavailable: {error}")
            })?;
        self.refresh_persistent_state_from_disk().await?;
        let Some(start_observation) = workspace_observation(&self.repository_root).await else {
            return Err(
                "the trusted runner could not capture the repository workspace fingerprint"
                    .to_string(),
            );
        };
        let start_fingerprint = start_observation.fingerprint.clone();
        let start_mutation_epoch = {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                Some(start_observation),
            )
            .await;
            state.persistent.mutation_epoch
        };
        self.persist().await?;
        let expected_inventory_hash = load_inventory_hash(&self.repository_root, &config).await?;
        let attempt_id = Uuid::now_v7().to_string();
        let nonce = Uuid::new_v4().as_simple().to_string();
        let report_path = self
            .persistence
            .attempts_dir
            .join(format!("{attempt_id}.json"));
        if tokio::fs::try_exists(&report_path).await.unwrap_or(true) {
            return Err("the private focused-validation report path was not fresh".to_string());
        }
        let runner_attestation = prepare_runner_attestation(&attempt_id, &nonce).await?;
        let expected_validation_ids = BTreeSet::from([validation_id.clone()]);
        let validation_evidence_contracts = BTreeMap::from([(
            validation_id.clone(),
            config
                .validation_evidence_contracts
                .get(&validation_id)
                .cloned()
                .ok_or_else(|| {
                    format!("focused validation {validation_id} has no evidence contract")
                })?,
        )]);
        let validation_path_patterns = BTreeMap::from([(
            validation_id.clone(),
            config
                .validation_path_patterns
                .get(&validation_id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "focused validation {validation_id} has no owned or consumed path mapping"
                    )
                })?,
        )]);
        let inner = CompletionProofAttempt {
            attempt_id,
            nonce,
            report_write_root: self.persistence.attempts_dir.clone(),
            report_path,
            repository_root: self.repository_root.clone(),
            exact_command: displayed_command.to_string(),
            start_fingerprint,
            start_mutation_epoch,
            parent_pid: std::process::id(),
            started_at_unix_ms: unix_time_ms(),
            expected_validation_ids,
            validation_evidence_contracts,
            validation_path_patterns,
            expected_policy_id: config.policy_id,
            policy_runner_bundle_sha256: authority.policy_runner_bundle_sha256,
            trusted_runner_entrypoints: authority.trusted_runner_entrypoints,
            expected_inventory_hash,
            expected_exceptions: BTreeSet::new(),
            expected_overrides: BTreeSet::new(),
            runner_attestation,
        };
        self.state
            .lock()
            .await
            .pending_attempts
            .insert(inner.attempt_id.clone(), inner.persisted_pending());
        Ok(Some(FocusedValidationAttempt {
            inner,
            validation_id,
        }))
    }

    pub(crate) async fn prepare_documentation_validation(
        &self,
        displayed_command: &str,
        cwd: &Path,
    ) -> Result<Option<DocumentationValidationAttempt>, String> {
        if !same_completion_proof_path(&canonical_repository_root(cwd), &self.repository_root) {
            return Ok(None);
        }
        let recognition_authority = match self.authority_for_command_recognition().await {
            Ok(authority) => authority,
            Err(_) => return Ok(None),
        };
        if recognition_authority
            .config
            .documentation_command
            .as_deref()
            != Some(displayed_command)
        {
            return Ok(None);
        }
        let authority = self.verified_authority().await?;
        let config = authority.config;
        if config.documentation_command.as_deref() != Some(displayed_command) {
            return Err(
                "the trusted documentation command changed while validation was being prepared"
                    .to_string(),
            );
        }

        let _operation = self.operation.lock().await;
        let _state_file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .map_err(|error| {
                format!("the private completion-proof state lock was unavailable: {error}")
            })?;
        self.refresh_persistent_state_from_disk().await?;
        let Some(observation) = workspace_observation(&self.repository_root).await else {
            return Err(
                "the runtime could not capture the repository workspace before documentation validation"
                    .to_string(),
            );
        };
        let attempt = {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                Some(observation.clone()),
            )
            .await;
            (state.persistent.requires_documentation_validation
                && !state.persistent.requires_non_documentation_proof)
                .then(|| DocumentationValidationAttempt {
                    exact_command: displayed_command.to_string(),
                    start_fingerprint: observation.fingerprint,
                    start_mutation_epoch: state.persistent.mutation_epoch,
                    policy_runner_bundle_sha256: authority.policy_runner_bundle_sha256,
                })
        };
        self.persist().await?;
        Ok(attempt)
    }

    pub(crate) async fn finish_documentation_validation(
        &self,
        attempt: DocumentationValidationAttempt,
        process_exit_code: Option<i32>,
    ) -> DocumentationValidationOutcome {
        let _operation = self.operation.lock().await;
        let _state_file_lock =
            match acquire_private_state_lock(self.persistence.lock_path.clone()).await {
                Ok(lock) => lock,
                Err(error) => {
                    return DocumentationValidationOutcome::PreResultError {
                        message: format!(
                            "the private completion-proof state lock was unavailable: {error}"
                        ),
                    };
                }
            };
        if let Err(message) = self.refresh_persistent_state_from_disk().await {
            return DocumentationValidationOutcome::PreResultError { message };
        }
        let authority = match self.verified_authority().await {
            Ok(authority) => authority,
            Err(error) => {
                return DocumentationValidationOutcome::PreResultError { message: error };
            }
        };
        if authority.policy_runner_bundle_sha256 != attempt.policy_runner_bundle_sha256 {
            return DocumentationValidationOutcome::PreResultError {
                message: "the trusted documentation policy changed during validation".to_string(),
            };
        }
        let config = authority.config;
        if config.documentation_command.as_deref() != Some(attempt.exact_command.as_str()) {
            return DocumentationValidationOutcome::PreResultError {
                message: "the repository documentation command changed during validation"
                    .to_string(),
            };
        }
        let current_observation = workspace_observation(&self.repository_root).await;
        let mut state = self.state.lock().await;
        reconcile_external_workspace_change(
            &self.repository_root,
            &mut state.persistent,
            current_observation.clone(),
        )
        .await;
        let outcome = if process_exit_code != Some(0) {
            DocumentationValidationOutcome::PreResultError {
                message: format!(
                    "the exact documentation command exited with {:?} instead of success",
                    process_exit_code
                ),
            }
        } else if current_observation
            .as_ref()
            .map(|observation| observation.fingerprint.as_str())
            != Some(attempt.start_fingerprint.as_str())
        {
            DocumentationValidationOutcome::PreResultError {
                message: "the repository changed while documentation validation was running"
                    .to_string(),
            }
        } else if state.persistent.mutation_epoch != attempt.start_mutation_epoch {
            DocumentationValidationOutcome::PreResultError {
                message: "the repository mutation epoch changed during documentation validation"
                    .to_string(),
            }
        } else if state.persistent.requires_non_documentation_proof {
            DocumentationValidationOutcome::PreResultError {
                message: "a non-documentation mutation now requires canonical certification"
                    .to_string(),
            }
        } else if !state.persistent.requires_documentation_validation {
            DocumentationValidationOutcome::PreResultError {
                message: "this invocation did not own a pending documentation validation"
                    .to_string(),
            }
        } else {
            state.persistent.requires_documentation_validation = false;
            DocumentationValidationOutcome::ConfirmedPass
        };
        drop(state);
        if let Err(message) = self.persist().await {
            return DocumentationValidationOutcome::PreResultError { message };
        }
        outcome
    }

    async fn record_confirmed_validation_failures(
        &self,
        pending: &PendingAttempt,
        validation_ids: &[String],
    ) {
        let validation_patterns = validation_ids
            .iter()
            .filter_map(|validation_id| {
                pending
                    .validation_path_patterns
                    .get(validation_id)
                    .cloned()
                    .map(|patterns| (validation_id.clone(), patterns))
            })
            .collect::<BTreeMap<_, _>>();
        let (failure_observation, failure_snapshots) =
            stable_validation_input_snapshots(&self.repository_root, &validation_patterns).await;
        let mut state = self.state.lock().await;
        reconcile_external_workspace_change(
            &self.repository_root,
            &mut state.persistent,
            failure_observation,
        )
        .await;
        let failed_at_mutation_epoch = state.persistent.mutation_epoch;
        for validation_id in validation_ids {
            state.persistent.poisoned_validations.insert(
                validation_id.clone(),
                PoisonedValidation {
                    validation_id: validation_id.clone(),
                    failed_at_mutation_epoch,
                    relevant_path_patterns: validation_patterns
                        .get(validation_id)
                        .map(ValidationInputContract::all_declared_patterns)
                        .unwrap_or_default(),
                    relevant_input_contract: validation_patterns.get(validation_id).cloned(),
                    failure_input_snapshot: failure_snapshots
                        .as_ref()
                        .and_then(|snapshots| snapshots.get(validation_id))
                        .cloned(),
                },
            );
        }
        state.persistent.registered_proof = None;
        state.persistent.requires_non_documentation_proof = true;
        state.persistent.requires_documentation_validation = false;
    }

    pub(crate) async fn finish_focused_attempt(
        &self,
        mut attempt: FocusedValidationAttempt,
        process_exit_code: Option<i32>,
    ) -> FocusedValidationOutcome {
        let _operation = self.operation.lock().await;
        let pending = self
            .state
            .lock()
            .await
            .pending_attempts
            .remove(&attempt.inner.attempt_id);
        let Some(pending) = pending else {
            return FocusedValidationOutcome::PreResultError {
                message: "the runtime no longer owned the private focused invocation nonce"
                    .to_string(),
            };
        };
        let runner_attestation = match attempt.inner.runner_attestation.receive().await {
            Ok(attestation) => attestation,
            Err(message) => {
                let _ = tokio::fs::remove_file(&pending.report_path).await;
                return FocusedValidationOutcome::PreResultError { message };
            }
        };
        let _state_file_lock =
            match acquire_private_state_lock(self.persistence.lock_path.clone()).await {
                Ok(lock) => lock,
                Err(error) => {
                    return FocusedValidationOutcome::PreResultError {
                        message: format!(
                            "the private completion-proof state lock was unavailable: {error}"
                        ),
                    };
                }
            };
        if let Err(message) = self.refresh_persistent_state_from_disk().await {
            return FocusedValidationOutcome::PreResultError { message };
        }
        let report_bytes = match tokio::fs::read(&pending.report_path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return FocusedValidationOutcome::PreResultError {
                    message: format!(
                        "the trusted focused runner did not produce its private report: {error}"
                    ),
                };
            }
        };
        let parsed_value = serde_json::from_slice::<serde_json::Value>(&report_bytes);
        let _ = tokio::fs::remove_file(&pending.report_path).await;
        let report_value = match parsed_value {
            Ok(report) => report,
            Err(error) => {
                return FocusedValidationOutcome::PreResultError {
                    message: format!("the private focused report was not valid JSON: {error}"),
                };
            }
        };
        if let Err(message) = validate_report_hashes(&report_value) {
            return FocusedValidationOutcome::PreResultError { message };
        }
        let report = match serde_json::from_value::<CompletionProofAttemptReportV1>(report_value) {
            Ok(report) => report,
            Err(error) => {
                return FocusedValidationOutcome::PreResultError {
                    message: format!(
                        "the private focused report did not match the trusted schema: {error}"
                    ),
                };
            }
        };
        if let Err(message) = validate_attempt_start_envelope(
            &pending,
            &report,
            FOCUSED_REPORT_TYPE,
            &runner_attestation,
        )
        .await
        {
            return FocusedValidationOutcome::PreResultError { message };
        }
        if report.focused_validation_id.as_deref() != Some(attempt.validation_id.as_str())
            || report.validations.len() != 1
            || report.validations[0].id != attempt.validation_id
            || report.child_processes.len() > 1
        {
            return FocusedValidationOutcome::PreResultError {
                message: "the focused report did not contain exactly the authorized validation"
                    .to_string(),
            };
        }

        let validation = &report.validations[0];
        if validation.classification == ValidationClassification::ConfirmedValidationFailure {
            if let Err(message) = validate_confirmed_failure(&pending, &report, validation).await {
                return FocusedValidationOutcome::PreResultError { message };
            }
            if report.child_processes.len() != 1 {
                return FocusedValidationOutcome::PreResultError {
                    message:
                        "the confirmed focused failure did not have exactly one fresh child runner"
                            .to_string(),
                };
            }
            self.record_confirmed_validation_failures(
                &pending,
                std::slice::from_ref(&attempt.validation_id),
            )
            .await;
            let _ = self.persist().await;
            return FocusedValidationOutcome::ConfirmedValidationFailure {
                validation_id: attempt.validation_id,
            };
        }

        if let Err(message) = self.verify_pending_authority(&pending).await {
            return FocusedValidationOutcome::PreResultError { message };
        }

        if let Err(message) = validate_attempt_completion_envelope(&pending, &report) {
            return FocusedValidationOutcome::PreResultError { message };
        }
        if report.attempt_classification != AttemptClassification::ConfirmedPass
            || validation.classification != ValidationClassification::ConfirmedPass
        {
            return FocusedValidationOutcome::PreResultError {
                message: if report.fatal_error.is_empty() {
                    "the trusted focused runner classified the attempt as a pre-result error"
                        .to_string()
                } else {
                    report.fatal_error.clone()
                },
            };
        }
        if process_exit_code != Some(0) {
            return FocusedValidationOutcome::PreResultError {
                message: format!(
                    "the exact focused process exited with {:?} instead of success",
                    process_exit_code
                ),
            };
        }
        if let Err(message) = validate_confirmed_pass(&pending, &report).await {
            return FocusedValidationOutcome::PreResultError { message };
        }
        if report.child_processes.len() != 1 {
            return FocusedValidationOutcome::PreResultError {
                message: "the confirmed focused pass did not have exactly one fresh child runner"
                    .to_string(),
            };
        }
        let current_observation = workspace_observation(&self.repository_root).await;
        let current_fingerprint = current_observation
            .as_ref()
            .map(|observation| observation.fingerprint.clone());
        if current_fingerprint.as_deref() != Some(report.end_fingerprint.as_str()) {
            self.note_external_non_documentation_mutation(current_fingerprint)
                .await;
            return FocusedValidationOutcome::PreResultError {
                message: "the repository changed after the focused runner recorded its ending fingerprint"
                    .to_string(),
            };
        }
        let poisoned_validation = {
            let state = self.state.lock().await;
            if state.persistent.mutation_epoch != pending.start_mutation_epoch {
                return FocusedValidationOutcome::PreResultError {
                    message: "the repository mutation epoch changed during focused validation"
                        .to_string(),
                };
            }
            state
                .persistent
                .poisoned_validations
                .get(&attempt.validation_id)
                .cloned()
        };
        if let Some(poisoned_validation) = poisoned_validation {
            match poisoned_validation_is_eligible_at_snapshot(
                &self.repository_root,
                &poisoned_validation,
                pending.start_mutation_epoch,
                &report.end_fingerprint,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return FocusedValidationOutcome::PreResultError {
                        message: format!(
                            "validation {} is poisoned at this mutation epoch; an unchanged, unrelated, or reverted retry cannot count",
                            attempt.validation_id
                        ),
                    };
                }
                Err(message) => {
                    return FocusedValidationOutcome::PreResultError { message };
                }
            }
        }
        FocusedValidationOutcome::ConfirmedPass {
            validation_id: attempt.validation_id,
        }
    }

    pub(crate) async fn finish_canonical_attempt(
        &self,
        mut attempt: CompletionProofAttempt,
        process_exit_code: Option<i32>,
        observed_workspace_change_paths: Option<Vec<PathBuf>>,
    ) -> CompletionProofAttemptOutcome {
        let _operation = self.operation.lock().await;
        let pending = self
            .state
            .lock()
            .await
            .pending_attempts
            .remove(&attempt.attempt_id);
        let Some(pending) = pending else {
            return CompletionProofAttemptOutcome::PreResultError {
                message: "the runtime no longer owned the private invocation nonce".to_string(),
            };
        };
        let runner_attestation = match attempt.runner_attestation.receive().await {
            Ok(attestation) => attestation,
            Err(message) => {
                let _ = tokio::fs::remove_file(&pending.report_path).await;
                return CompletionProofAttemptOutcome::PreResultError { message };
            }
        };
        let _state_file_lock =
            match acquire_private_state_lock(self.persistence.lock_path.clone()).await {
                Ok(lock) => lock,
                Err(error) => {
                    return CompletionProofAttemptOutcome::PreResultError {
                        message: format!(
                            "the private completion-proof state lock was unavailable: {error}"
                        ),
                    };
                }
            };
        if let Err(message) = self.refresh_persistent_state_from_disk().await {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }
        let report_bytes = match tokio::fs::read(&pending.report_path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: format!(
                        "the trusted runner did not produce its private report: {error}"
                    ),
                };
            }
        };
        let parsed_value = serde_json::from_slice::<serde_json::Value>(&report_bytes);
        let _ = tokio::fs::remove_file(&pending.report_path).await;
        let report_value = match parsed_value {
            Ok(report) => report,
            Err(error) => {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: format!("the private report was not valid JSON: {error}"),
                };
            }
        };
        if let Err(message) = validate_report_hashes(&report_value) {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }
        let report = match serde_json::from_value::<CompletionProofAttemptReportV1>(report_value) {
            Ok(report) => report,
            Err(error) => {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: format!(
                        "the private report did not match the trusted schema: {error}"
                    ),
                };
            }
        };
        if let Err(message) = validate_attempt_start_envelope(
            &pending,
            &report,
            ATTEMPT_REPORT_TYPE,
            &runner_attestation,
        )
        .await
        {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }

        let mut confirmed_failures = Vec::new();
        let mut malformed_failure = None;
        for validation in report.validations.iter().filter(|validation| {
            validation.classification == ValidationClassification::ConfirmedValidationFailure
        }) {
            match validate_confirmed_failure(&pending, &report, validation).await {
                Ok(()) => confirmed_failures.push(validation.id.clone()),
                Err(message) => {
                    malformed_failure.get_or_insert(message);
                }
            }
        }
        if !confirmed_failures.is_empty() {
            self.record_confirmed_validation_failures(&pending, &confirmed_failures)
                .await;
            let _ = self.persist().await;
            return CompletionProofAttemptOutcome::ConfirmedValidationFailure {
                validation_ids: confirmed_failures,
            };
        }
        if let Some(message) = malformed_failure {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }

        if let Err(message) = self.verify_pending_authority(&pending).await {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }

        if let Err(message) = validate_attempt_completion_envelope(&pending, &report) {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }

        if report.attempt_classification != AttemptClassification::ConfirmedPass {
            return CompletionProofAttemptOutcome::PreResultError {
                message: "the trusted runner classified the attempt as a pre-result error"
                    .to_string(),
            };
        }
        if process_exit_code != Some(0) {
            return CompletionProofAttemptOutcome::PreResultError {
                message: format!(
                    "the canonical process exited with {:?} instead of success",
                    process_exit_code
                ),
            };
        }
        if let Err(message) = validate_confirmed_pass(&pending, &report).await {
            return CompletionProofAttemptOutcome::PreResultError { message };
        }
        let Some(observed_workspace_change_paths) = observed_workspace_change_paths else {
            return CompletionProofAttemptOutcome::PreResultError {
                message: "the trusted workspace watcher could not establish that the repository remained unchanged during certification"
                    .to_string(),
            };
        };
        match canonical_attempt_observed_relevant_change(
            &self.repository_root,
            &observed_workspace_change_paths,
        )
        .await
        {
            Some(false) => {}
            Some(true) => {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: "the trusted workspace watcher observed repository content or existence change during certification; restoring the ending bytes does not make that attempt valid"
                        .to_string(),
                };
            }
            None => {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: "the trusted workspace watcher could not classify repository changes observed during certification"
                        .to_string(),
                };
            }
        }
        let current_observation = workspace_observation(&self.repository_root).await;
        let current_fingerprint = current_observation
            .as_ref()
            .map(|observation| observation.fingerprint.clone());
        if current_fingerprint.as_deref() != Some(report.end_fingerprint.as_str()) {
            self.note_external_non_documentation_mutation(current_fingerprint)
                .await;
            return CompletionProofAttemptOutcome::PreResultError {
                message: "the repository changed after the runner recorded its ending fingerprint"
                    .to_string(),
            };
        }
        let poisoned_validations = {
            let state = self.state.lock().await;
            if state.persistent.mutation_epoch != pending.start_mutation_epoch {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: "the repository mutation epoch changed during certification"
                        .to_string(),
                };
            }
            state
                .persistent
                .poisoned_validations
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut poison_ids_to_retire = BTreeSet::new();
        for poisoned_validation in &poisoned_validations {
            if !pending
                .expected_validation_ids
                .contains(&poisoned_validation.validation_id)
            {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: format!(
                        "validation {} remains poisoned but is absent from the current canonical validation set; certification cannot omit a previously failed required validation",
                        poisoned_validation.validation_id
                    ),
                };
            }
            match poisoned_validation_is_eligible_at_snapshot(
                &self.repository_root,
                poisoned_validation,
                pending.start_mutation_epoch,
                &report.end_fingerprint,
            )
            .await
            {
                Ok(true) => {
                    poison_ids_to_retire.insert(poisoned_validation.validation_id.clone());
                }
                Ok(false) => {
                    return CompletionProofAttemptOutcome::PreResultError {
                        message: format!(
                            "validation {} is poisoned at this mutation epoch; an unchanged, unrelated, or reverted retry cannot become proof",
                            poisoned_validation.validation_id
                        ),
                    };
                }
                Err(message) => {
                    return CompletionProofAttemptOutcome::PreResultError { message };
                }
            }
        }
        let report_hash = format!("{:x}", Sha256::digest(&report_bytes));
        let mut artifact = CompletionProofArtifactV1 {
            schema_version: 1,
            artifact_type: "CompletionProofArtifactV1".to_string(),
            policy_id: pending.expected_policy_id.clone(),
            policy_runner_bundle_sha256: pending.policy_runner_bundle_sha256.clone(),
            exact_command: pending.exact_command.clone(),
            repository_root: pending.repository_root.to_string_lossy().into_owned(),
            host_identity: report.host_identity.clone(),
            invocation_nonce: pending.nonce.clone(),
            parent_pid: pending.parent_pid,
            observed_runner_parent_pid: report.observed_runner_parent_pid,
            start_fingerprint: pending.start_fingerprint.clone(),
            end_fingerprint: report.end_fingerprint.clone(),
            workspace_fingerprint: report.end_fingerprint.clone(),
            start_mutation_epoch: pending.start_mutation_epoch,
            end_mutation_epoch: report.end_mutation_epoch,
            mutation_epoch: pending.start_mutation_epoch,
            inventory_hash: report.inventory_hash.clone(),
            report_hash,
            attempt_report_hash: report.attempt_report_hash.clone(),
            attempt_id: pending.attempt_id.clone(),
            validations: report.validations.clone(),
            child_processes: report.child_processes.clone(),
            exceptions: report.exceptions.clone(),
            overrides: report.overrides.clone(),
            registered_at_unix_ms: unix_time_ms(),
            artifact_hash: String::new(),
        };
        artifact.artifact_hash = match completion_artifact_hash(&artifact) {
            Ok(hash) => hash,
            Err(message) => {
                return CompletionProofAttemptOutcome::PreResultError { message };
            }
        };
        {
            let mut state = self.state.lock().await;
            if state.persistent.mutation_epoch != pending.start_mutation_epoch {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: "the repository mutation epoch changed during certification"
                        .to_string(),
                };
            }
            if let Some(missing_poison) = poison_ids_to_retire.iter().find(|validation_id| {
                !state
                    .persistent
                    .poisoned_validations
                    .contains_key(*validation_id)
            }) {
                return CompletionProofAttemptOutcome::PreResultError {
                    message: format!(
                        "the poisoned validation state for {missing_poison} changed during certification"
                    ),
                };
            }
            for validation_id in &poison_ids_to_retire {
                state.persistent.poisoned_validations.remove(validation_id);
            }
            state.persistent.registered_proof = Some(artifact);
            state.persistent.last_observed_fingerprint = Some(report.end_fingerprint);
            if let Some(observation) = current_observation {
                state.persistent.last_observed_head_identity = observation.head_identity;
                state.persistent.last_observed_head_identity_initialized = true;
                state.persistent.last_observed_path_fingerprints = observation.path_fingerprints;
            } else {
                state.persistent.last_observed_head_identity = None;
                state.persistent.last_observed_head_identity_initialized = false;
                state.persistent.last_observed_path_fingerprints.clear();
            }
            state.persistent.requires_non_documentation_proof = false;
            state.persistent.requires_documentation_validation = false;
        }
        if let Err(message) = self.persist_and_install_live_issuance().await {
            self.revoke_live_issuance().await;
            return CompletionProofAttemptOutcome::PreResultError { message };
        }
        CompletionProofAttemptOutcome::ConfirmedPass
    }

    pub(crate) async fn note_mutation_paths(&self, cwd: &Path, _paths: Option<&BTreeSet<PathBuf>>) {
        if !completion_proof_path_identity(&canonical_repository_root(cwd))
            .starts_with(completion_proof_path_identity(&self.repository_root))
        {
            return;
        }
        let _operation = self.operation.lock().await;
        let _state_file_lock = match acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
        {
            Ok(lock) => lock,
            Err(error) => {
                self.block_persistent_state(format!(
                    "the private completion-proof state lock was unavailable while recording a mutation: {error}"
                ))
                .await;
                return;
            }
        };
        if self.refresh_persistent_state_from_disk().await.is_err() {
            return;
        }
        let current_observation = workspace_observation(&self.repository_root).await;
        {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                current_observation,
            )
            .await;
        }
        let _ = self.persist().await;
    }

    pub(crate) async fn begin_post_proof_mutation_observation(
        &self,
        cwd: &Path,
    ) -> Option<PostProofMutationObservation> {
        if !same_completion_proof_path(&canonical_repository_root(cwd), &self.repository_root) {
            return None;
        }
        let _operation = self.operation.lock().await;
        let _state_file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .ok()?;
        self.refresh_persistent_state_from_disk().await.ok()?;
        let current_observation = workspace_observation(&self.repository_root).await;
        let token =
            {
                let mut state = self.state.lock().await;
                reconcile_external_workspace_change(
                    &self.repository_root,
                    &mut state.persistent,
                    current_observation,
                )
                .await;
                state
                    .persistent
                    .registered_proof
                    .as_ref()
                    .and_then(|proof| {
                        state.persistent.last_observed_fingerprint.as_ref().map(
                            |start_fingerprint| PostProofMutationObservation {
                                repository_root: self.repository_root.clone(),
                                start_mutation_epoch: state.persistent.mutation_epoch,
                                start_fingerprint: start_fingerprint.clone(),
                                proof_artifact_hash: proof.artifact_hash.clone(),
                            },
                        )
                    })
            };
        self.persist().await.ok()?;
        let token = token?;
        let key = self.live_issuance_key()?;
        let is_live = self
            .runtime_registry
            .live_issuances
            .lock()
            .await
            .get(&key)
            .is_some_and(|issuance| issuance.artifact_hash == token.proof_artifact_hash);
        is_live.then_some(token)
    }

    pub(crate) async fn bind_current_proof_source_observation(
        &self,
        source_observation: crate::git_workspace::SourcePathChangeObservation,
        source_workspace: Arc<crate::git_workspace::GitWorkspaceCache>,
    ) {
        let _transition = self
            .runtime_registry
            .post_proof_source_transition
            .lock()
            .await;
        self.bind_current_proof_source_observation_inner(source_observation, source_workspace)
            .await;
    }

    async fn bind_current_proof_source_observation_inner(
        &self,
        source_observation: crate::git_workspace::SourcePathChangeObservation,
        source_workspace: Arc<crate::git_workspace::GitWorkspaceCache>,
    ) {
        let proof_observation = self
            .begin_post_proof_mutation_observation(&self.repository_root)
            .await;
        let Some(key) = self.live_issuance_key() else {
            return;
        };
        let mut observations = self
            .runtime_registry
            .post_proof_source_observations
            .lock()
            .await;
        match proof_observation {
            Some(proof_observation) => {
                observations.insert(
                    key,
                    BoundPostProofSourceObservation {
                        proof_observation,
                        source_observation,
                        source_workspace,
                    },
                );
            }
            None => {
                observations.remove(&key);
            }
        }
    }

    pub(crate) async fn refresh_post_proof_source_observation(
        &self,
        _git_workspace: &crate::git_workspace::GitWorkspaceCache,
    ) {
        let _transition = self
            .runtime_registry
            .post_proof_source_transition
            .lock()
            .await;
        let previous = if let Some(key) = self.live_issuance_key() {
            self.runtime_registry
                .post_proof_source_observations
                .lock()
                .await
                .remove(&key)
        } else {
            None
        };
        let Some(previous) = previous else {
            // Persisted state is not live issuance. A new manager therefore blocks at the gate
            // without converting the restart into a mutation and without launching validation.
            return;
        };

        // Finish and continue from one journal snapshot. The prior observation
        // stays valid through the barrier, and the continuation starts at that
        // exact generation without a nested barrier or unchecked interval.
        let source_workspace = Arc::clone(&previous.source_workspace);
        let (observed_paths, replacement) = match source_workspace
            .finish_source_path_change_observation_and_continue(&previous.source_observation)
            .await
        {
            Some(handoff) => {
                let observed_paths = handoff.classifiable_exact_paths();
                (observed_paths, Some(handoff.continuation))
            }
            None => (None, None),
        };
        self.finish_post_proof_mutation_observation(previous.proof_observation, observed_paths)
            .await;

        match replacement {
            Some(source_observation) => {
                self.bind_current_proof_source_observation_inner(
                    source_observation,
                    source_workspace,
                )
                .await;
            }
            None => {
                if let Some(proof_observation) = self
                    .begin_post_proof_mutation_observation(&self.repository_root)
                    .await
                {
                    self.finish_post_proof_mutation_observation(proof_observation, None)
                        .await;
                }
            }
        }
    }

    pub(crate) async fn finish_post_proof_mutation_observation(
        &self,
        observation: PostProofMutationObservation,
        observed_paths: Option<Vec<PathBuf>>,
    ) {
        if observation.repository_root != self.repository_root {
            return;
        }
        let _operation = self.operation.lock().await;
        let _state_file_lock = match acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
        {
            Ok(lock) => lock,
            Err(error) => {
                self.block_persistent_state(format!(
                    "the private completion-proof state lock was unavailable while finishing mutation observation: {error}"
                ))
                .await;
                return;
            }
        };
        if self.refresh_persistent_state_from_disk().await.is_err() {
            return;
        }
        let relevant_paths = match observed_paths {
            Some(paths) => observed_relevant_paths(&self.repository_root, &paths).await,
            None => None,
        };
        let current_observation = workspace_observation(&self.repository_root).await;
        let prior_artifact = self.state.lock().await.persistent.registered_proof.clone();
        let prior_was_live = match prior_artifact.as_ref() {
            Some(artifact) => self.has_exact_live_issuance(artifact).await,
            None => false,
        };
        let change_kind = {
            let mut state = self.state.lock().await;
            let token_is_current = state.persistent.mutation_epoch
                == observation.start_mutation_epoch
                && state.persistent.last_observed_fingerprint.as_deref()
                    == Some(observation.start_fingerprint.as_str())
                && state
                    .persistent
                    .registered_proof
                    .as_ref()
                    .is_some_and(|proof| proof.artifact_hash == observation.proof_artifact_hash);
            if !token_is_current {
                None
            } else {
                match relevant_paths {
                    Some(paths) if paths.is_empty() => None,
                    Some(paths) if paths.iter().all(|path| is_documentation(path)) => {
                        state.persistent.requires_documentation_validation = true;
                        if let (Some(proof), Some(current_observation)) = (
                            state.persistent.registered_proof.as_mut(),
                            current_observation.as_ref(),
                        ) {
                            proof.workspace_fingerprint = current_observation.fingerprint.clone();
                            refresh_completion_artifact_hash(proof);
                        }
                        Some(true)
                    }
                    Some(_) | None => {
                        state.persistent.mutation_epoch =
                            state.persistent.mutation_epoch.saturating_add(1);
                        state.persistent.requires_non_documentation_proof = true;
                        state.persistent.requires_documentation_validation = false;
                        state.persistent.registered_proof = None;
                        Some(false)
                    }
                }
            }
        };
        let Some(documentation_rebind) = change_kind else {
            return;
        };
        if let Some(current_observation) = current_observation {
            let mut state = self.state.lock().await;
            state.persistent.last_observed_fingerprint = Some(current_observation.fingerprint);
            state.persistent.last_observed_head_identity = current_observation.head_identity;
            state.persistent.last_observed_head_identity_initialized = true;
            state.persistent.last_observed_path_fingerprints =
                current_observation.path_fingerprints;
        }
        if self.persist().await.is_err() {
            self.revoke_live_issuance().await;
        } else if documentation_rebind && prior_was_live {
            if self.persist_and_install_live_issuance().await.is_err() {
                self.revoke_live_issuance().await;
            }
        } else {
            self.revoke_live_issuance().await;
        }
    }

    pub(crate) async fn check_gate_with_workspace(
        &self,
        git_workspace: &crate::git_workspace::GitWorkspaceCache,
    ) -> CompletionProofGateDecision {
        self.refresh_post_proof_source_observation(git_workspace)
            .await;
        self.check_gate().await
    }

    pub(crate) async fn check_gate(&self) -> CompletionProofGateDecision {
        // Descendant agents return evidence to their root session; they never publish the
        // user-visible terminal result that this gate protects. Keep their focused-validation
        // tracking active, while leaving whole-repository certification and terminal gating
        // exclusively with the root session.
        if !self.is_terminal_owner() {
            return CompletionProofGateDecision::Accepted;
        }
        let _operation = self.operation.lock().await;
        let _state_file_lock = match acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
        {
            Ok(lock) => lock,
            Err(error) => {
                return CompletionProofGateDecision::Blocked {
                    message: format!(
                        "Successful completion is blocked because the private completion-proof state lock is unavailable: {error}"
                    ),
                };
            }
        };
        if let Err(error) = self.refresh_persistent_state_from_disk().await {
            return CompletionProofGateDecision::Blocked {
                message: format!(
                    "Successful completion is blocked because private completion-proof state could not be authenticated: {error}"
                ),
            };
        }
        let current_observation = workspace_observation(&self.repository_root).await;
        let current_fingerprint = current_observation
            .as_ref()
            .map(|observation| observation.fingerprint.clone());
        let repository_authority = self.verified_authority().await;
        let expected_inventory_hash = match repository_authority.as_ref() {
            Ok(authority) => load_inventory_hash(&self.repository_root, &authority.config)
                .await
                .ok(),
            Err(_) => None,
        };
        let repository_relaxation =
            verified_repository_relaxation(&self.repository_root, &self.repository_relaxation)
                .await;
        let live_issuance = if let Some(key) = self.live_issuance_key() {
            self.runtime_registry
                .live_issuances
                .lock()
                .await
                .get(&key)
                .cloned()
        } else {
            None
        };
        let decision = {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                current_observation,
            )
            .await;
            let proof_is_current = if let (Some(proof), Some(fingerprint), Some(inventory_hash)) = (
                state.persistent.registered_proof.as_ref(),
                current_fingerprint.as_ref(),
                expected_inventory_hash.as_ref(),
            ) {
                repository_authority.as_ref().ok().is_some_and(|authority| {
                    completion_artifact_is_current(
                        proof,
                        &authority.config,
                        &self.repository_root,
                        fingerprint,
                        inventory_hash,
                        state.persistent.mutation_epoch,
                        &authority.policy_runner_bundle_sha256,
                    )
                }) && state.persistent.poisoned_validations.is_empty()
                    && live_issuance.as_ref().is_some_and(|issuance| {
                        Self::live_issuance_matches_artifact(issuance, proof)
                    })
            } else {
                false
            };
            let current_user_relaxation =
                self.session_lineage_id
                    .as_deref()
                    .and_then(|session_lineage_id| {
                        state
                            .persistent
                            .current_user_relaxations_by_lineage
                            .get(session_lineage_id)
                            .or_else(|| {
                                state.persistent.current_user_relaxation.as_ref().filter(
                                    |relaxation| {
                                        relaxation.session_lineage_id == session_lineage_id
                                    },
                                )
                            })
                    });
            let relaxation = effective_completion_proof_relaxation(
                current_user_relaxation,
                self.session_lineage_id.as_deref(),
                repository_relaxation,
            );
            let (applied_relaxation, relaxation_rejection) = match relaxation {
                EffectiveCompletionProofRelaxation::Allowed(applied) => (Some(applied), None),
                EffectiveCompletionProofRelaxation::NotAllowed { reason } => (None, reason),
            };
            let requires_documentation_validation =
                state.persistent.requires_documentation_validation
                    && !state.persistent.requires_non_documentation_proof;
            // A committed artifact is usable only while this exact process-private root lineage
            // still owns its matching live issuance. Persisted state deliberately survives a
            // restart for diagnostics, but it cannot make a copied rollout or a fresh runtime
            // authoritative merely because certification previously cleared the mutation flag.
            let requires_non_documentation_proof =
                state.persistent.requires_non_documentation_proof
                    || (state.persistent.registered_proof.is_some() && !proof_is_current);
            let configured_documentation_command = requires_documentation_validation
                .then(|| {
                    repository_authority
                        .as_ref()
                        .ok()
                        .and_then(|authority| authority.config.documentation_command.as_deref())
                })
                .flatten();
            let needs_relaxation = (requires_documentation_validation
                && configured_documentation_command.is_none())
                || requires_non_documentation_proof;
            state.persistent.last_applied_relaxation = if needs_relaxation {
                applied_relaxation.clone()
            } else {
                None
            };
            let rejection_suffix = relaxation_rejection
                .as_deref()
                .map(|reason| format!(" {reason}"))
                .unwrap_or_default();

            if let Some(documentation_command) = configured_documentation_command {
                CompletionProofGateDecision::Blocked {
                    message: format!(
                        "Documentation changed. Run the repository's exact configured documentation validation from its root: `{documentation_command}`. A completion-proof relaxation cannot replace a configured documentation validation. The CompletionProofGate will not run validation for you.{rejection_suffix}"
                    ),
                }
            } else if needs_relaxation && applied_relaxation.is_some() {
                CompletionProofGateDecision::Accepted
            } else if requires_documentation_validation {
                CompletionProofGateDecision::Blocked {
                    message: format!(
                        "Documentation changed. Run the nearest existing documentation validation, or provide an explicit provenance-backed override if the repository has none. The CompletionProofGate will not run validation for you.{rejection_suffix}"
                    ),
                }
            } else if proof_is_current {
                CompletionProofGateDecision::Accepted
            } else if !requires_non_documentation_proof {
                CompletionProofGateDecision::Accepted
            } else if let Ok(authority) = repository_authority.as_ref() {
                CompletionProofGateDecision::Blocked {
                    message: format!(
                        "Successful completion is blocked until you explicitly run the repository's exact canonical proof command from its root: `{}`. Focused checks and separately run constituent commands do not count. The CompletionProofGate only verifies the private result and will not launch tests.{rejection_suffix}",
                        authority.config.canonical_command,
                    ),
                }
            } else {
                let config_error = repository_authority
                    .as_ref()
                    .err()
                    .map(String::as_str)
                    .unwrap_or("the repository did not provide a trusted configuration");
                CompletionProofGateDecision::Blocked {
                    message: format!(
                        "Successful completion is blocked because this repository has non-documentation changes but does not define a valid explicitly trusted canonical completion-proof command: {config_error}. The CompletionProofGate will not invent or launch one; a clear repository instruction or current-user override is required.{rejection_suffix}"
                    ),
                }
            }
        };
        if let Err(error) = self.persist().await {
            CompletionProofGateDecision::Blocked {
                message: format!(
                    "Successful completion is blocked because private completion-proof state could not be committed: {error}"
                ),
            }
        } else {
            decision
        }
    }

    async fn note_external_non_documentation_mutation(&self, current_fingerprint: Option<String>) {
        let current_observation = workspace_observation(&self.repository_root).await;
        {
            let mut state = self.state.lock().await;
            state.persistent.mutation_epoch = state.persistent.mutation_epoch.saturating_add(1);
            state.persistent.last_observed_fingerprint = current_observation
                .as_ref()
                .map(|observation| observation.fingerprint.clone())
                .or(current_fingerprint);
            if let Some(observation) = current_observation {
                state.persistent.last_observed_head_identity = observation.head_identity;
                state.persistent.last_observed_head_identity_initialized = true;
                state.persistent.last_observed_path_fingerprints = observation.path_fingerprints;
            } else {
                state.persistent.last_observed_head_identity = None;
                state.persistent.last_observed_head_identity_initialized = false;
                state.persistent.last_observed_path_fingerprints.clear();
            }
            state.persistent.requires_non_documentation_proof = true;
            state.persistent.requires_documentation_validation = false;
            state.persistent.registered_proof = None;
        }
        let _ = self.persist().await;
    }

    async fn refresh_persistent_state_from_disk(&self) -> Result<(), String> {
        if let ProofStateIntegrity::Blocked(error) = &self.state.lock().await.integrity {
            return Err(error.clone());
        }
        match load_authenticated_state(&self.persistence, &self.repository_root).await {
            Ok((persistent, seal)) => {
                let mut state = self.state.lock().await;
                state.persistent = persistent;
                state.integrity = ProofStateIntegrity::Trusted(seal);
                Ok(())
            }
            Err(error) => {
                self.block_persistent_state(error.clone()).await;
                Err(error)
            }
        }
    }

    async fn persist(&self) -> Result<(), String> {
        let (persistent, seal) = {
            let state = self.state.lock().await;
            let ProofStateIntegrity::Trusted(seal) = &state.integrity else {
                let ProofStateIntegrity::Blocked(error) = &state.integrity else {
                    unreachable!();
                };
                return Err(format!(
                    "the private completion-proof state is blocked: {error}"
                ));
            };
            (state.persistent.clone(), seal.clone())
        };

        let result = persist_authenticated_state(&self.persistence, persistent, &seal).await;
        match result {
            Ok(updated_seal) => {
                self.state.lock().await.integrity = ProofStateIntegrity::Trusted(updated_seal);
                Ok(())
            }
            Err(error) => {
                self.block_persistent_state(error.clone()).await;
                Err(error)
            }
        }
    }

    async fn block_persistent_state(&self, error: String) {
        tracing::warn!(%error, "completion-proof persistent state failed closed");
        let mut state = self.state.lock().await;
        state.persistent.requires_non_documentation_proof = true;
        state.persistent.requires_documentation_validation = false;
        state.persistent.registered_proof = None;
        state.integrity = ProofStateIntegrity::Blocked(error);
    }
}

fn valid_exact_identity(value: String) -> Option<String> {
    (!value.is_empty()
        && value.trim() == value
        && value.len() <= 1024
        && !value.chars().any(char::is_control))
    .then_some(value)
}

fn parse_relaxation_directives(
    text: &str,
    source: &str,
    provenance: &str,
) -> CompletionProofRelaxationResolution {
    if valid_exact_identity(source.to_string()).is_none()
        || valid_exact_identity(provenance.to_string()).is_none()
    {
        return CompletionProofRelaxationResolution::Invalid {
            message: "completion-proof relaxation provenance was not exact and nonempty"
                .to_string(),
        };
    }

    let mut directives = Vec::new();
    let mut decisions = BTreeSet::new();
    let mut markdown_fence = None;
    for exact_line in text.lines() {
        let mut candidate = exact_line.trim();
        if let Some(marker) = markdown_fence {
            if markdown_fence_marker(candidate) == Some(marker) {
                markdown_fence = None;
            }
            continue;
        }
        if let Some(marker) = markdown_fence_marker(candidate) {
            markdown_fence = Some(marker);
            continue;
        }
        if candidate.starts_with('>') || markdown_indented_code_line(exact_line) {
            continue;
        }
        if let Some(stripped) = candidate
            .strip_prefix("- ")
            .or_else(|| candidate.strip_prefix("* "))
            .or_else(|| candidate.strip_prefix("+ "))
        {
            candidate = stripped.trim();
        }
        candidate = candidate
            .strip_suffix('.')
            .or_else(|| candidate.strip_suffix('!'))
            .unwrap_or(candidate)
            .trim_end();
        let decision = if candidate.eq_ignore_ascii_case(RELAXATION_ALLOW_DIRECTIVE) {
            Some(CompletionProofRelaxationDecision::AllowWithoutCurrentProof)
        } else if candidate.eq_ignore_ascii_case(RELAXATION_REQUIRE_DIRECTIVE) {
            Some(CompletionProofRelaxationDecision::RequireCurrentProof)
        } else {
            None
        };
        let Some(decision) = decision else {
            continue;
        };
        decisions.insert(decision);
        directives.push(CompletionOverrideEvidence {
            text: exact_line.to_string(),
            source: source.to_string(),
            provenance: provenance.to_string(),
        });
    }

    directives.sort();
    directives.dedup();
    match decisions.len() {
        0 => CompletionProofRelaxationResolution::None,
        1 => CompletionProofRelaxationResolution::Clear {
            decision: *decisions.iter().next().expect("one relaxation decision"),
            evidence: directives,
        },
        _ => CompletionProofRelaxationResolution::Ambiguous {
            evidence: directives,
        },
    }
}

fn markdown_fence_marker(line: &str) -> Option<char> {
    let marker = line.chars().next()?;
    if !matches!(marker, '`' | '~') || line.chars().take_while(|value| *value == marker).count() < 3
    {
        return None;
    }
    Some(marker)
}

fn markdown_indented_code_line(line: &str) -> bool {
    if line.starts_with('\t') {
        return true;
    }
    let leading_spaces = line.bytes().take_while(|value| *value == b' ').count();
    leading_spaces >= 4 && !matches!(line.trim_start().as_bytes(), [b'-' | b'*' | b'+', b' ', ..])
}

fn merge_relaxation_resolutions(
    resolutions: impl IntoIterator<Item = CompletionProofRelaxationResolution>,
) -> CompletionProofRelaxationResolution {
    let mut decisions = BTreeSet::new();
    let mut evidence = Vec::new();
    for resolution in resolutions {
        match resolution {
            CompletionProofRelaxationResolution::None => {}
            CompletionProofRelaxationResolution::Clear {
                decision,
                evidence: mut resolution_evidence,
            } => {
                decisions.insert(decision);
                evidence.append(&mut resolution_evidence);
            }
            CompletionProofRelaxationResolution::Ambiguous {
                evidence: mut resolution_evidence,
            } => {
                evidence.append(&mut resolution_evidence);
                evidence.sort();
                evidence.dedup();
                return CompletionProofRelaxationResolution::Ambiguous { evidence };
            }
            CompletionProofRelaxationResolution::Invalid { message } => {
                return CompletionProofRelaxationResolution::Invalid { message };
            }
        }
    }
    evidence.sort();
    evidence.dedup();
    match decisions.len() {
        0 => CompletionProofRelaxationResolution::None,
        1 => CompletionProofRelaxationResolution::Clear {
            decision: *decisions.iter().next().expect("one relaxation decision"),
            evidence,
        },
        _ => CompletionProofRelaxationResolution::Ambiguous { evidence },
    }
}

async fn capture_repository_relaxation(repository_root: &Path) -> RepositoryRelaxationSnapshot {
    let head_oid = match git_text(repository_root, &["rev-parse", "--verify", "HEAD"]).await {
        Ok(value) if is_git_oid(&value) => value,
        Ok(_) => {
            return RepositoryRelaxationSnapshot {
                resolution: CompletionProofRelaxationResolution::Invalid {
                    message: "repository completion-proof instruction provenance had an invalid HEAD identity"
                        .to_string(),
                },
                ..RepositoryRelaxationSnapshot::default()
            };
        }
        Err(_) => return RepositoryRelaxationSnapshot::default(),
    };
    let tree = match git_bytes(repository_root, &["ls-tree", "-r", "-z", head_oid.as_str()]).await {
        Ok(tree) => tree,
        Err(message) => {
            return RepositoryRelaxationSnapshot {
                head_oid: Some(head_oid),
                resolution: CompletionProofRelaxationResolution::Invalid { message },
                ..RepositoryRelaxationSnapshot::default()
            };
        }
    };

    let mut candidates_by_directory = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut instruction_paths = BTreeSet::new();
    for record in tree
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return RepositoryRelaxationSnapshot {
                head_oid: Some(head_oid),
                resolution: CompletionProofRelaxationResolution::Invalid {
                    message: "repository instruction tree evidence was malformed".to_string(),
                },
                ..RepositoryRelaxationSnapshot::default()
            };
        };
        let metadata = String::from_utf8_lossy(&record[..tab]);
        let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
        let Some(filename) = repository_instruction_filename(&path) else {
            continue;
        };
        if !metadata.starts_with("100644 blob ") && !metadata.starts_with("100755 blob ") {
            return RepositoryRelaxationSnapshot {
                head_oid: Some(head_oid),
                resolution: CompletionProofRelaxationResolution::Invalid {
                    message: format!(
                        "repository completion-proof instruction {path} was not a regular committed file"
                    ),
                },
                ..RepositoryRelaxationSnapshot::default()
            };
        }
        instruction_paths.insert(path.clone());
        let directory = path
            .rsplit_once('/')
            .map(|(directory, _)| directory.to_string())
            .unwrap_or_default();
        candidates_by_directory
            .entry(directory)
            .or_default()
            .insert(filename.to_string(), path);
    }

    let effective_paths = candidates_by_directory
        .values()
        .filter_map(|candidates| {
            REPOSITORY_INSTRUCTION_FILENAMES
                .iter()
                .find_map(|filename| candidates.get(*filename).cloned())
        })
        .collect::<Vec<_>>();
    let Some(root_instruction_path) = candidates_by_directory.get("").and_then(|candidates| {
        REPOSITORY_INSTRUCTION_FILENAMES
            .iter()
            .find_map(|filename| candidates.get(*filename).cloned())
    }) else {
        return RepositoryRelaxationSnapshot {
            head_oid: Some(head_oid),
            instruction_paths,
            resolution: CompletionProofRelaxationResolution::None,
        };
    };

    let mut resolutions = Vec::new();
    let mut root_resolution = CompletionProofRelaxationResolution::None;
    for path in effective_paths {
        let contents = match git_bytes(repository_root, &["show", &format!("{head_oid}:{path}")])
            .await
        {
            Ok(contents) => match String::from_utf8(contents) {
                Ok(contents) => contents,
                Err(_) => {
                    return RepositoryRelaxationSnapshot {
                        head_oid: Some(head_oid),
                        instruction_paths,
                        resolution: CompletionProofRelaxationResolution::Invalid {
                            message: format!(
                                "repository completion-proof instruction {path} was not UTF-8 text"
                            ),
                        },
                    };
                }
            },
            Err(message) => {
                return RepositoryRelaxationSnapshot {
                    head_oid: Some(head_oid),
                    instruction_paths,
                    resolution: CompletionProofRelaxationResolution::Invalid { message },
                };
            }
        };
        let provenance = format!("repository:HEAD:{head_oid}:{path}");
        let resolution = parse_relaxation_directives(&contents, &path, &provenance);
        if path == root_instruction_path {
            root_resolution = resolution.clone();
        }
        resolutions.push(resolution);
    }

    if matches!(root_resolution, CompletionProofRelaxationResolution::None) {
        return RepositoryRelaxationSnapshot {
            head_oid: Some(head_oid),
            instruction_paths,
            resolution: CompletionProofRelaxationResolution::None,
        };
    }
    RepositoryRelaxationSnapshot {
        head_oid: Some(head_oid),
        instruction_paths,
        resolution: merge_relaxation_resolutions(resolutions),
    }
}

async fn verified_repository_relaxation(
    repository_root: &Path,
    snapshot: &RepositoryRelaxationSnapshot,
) -> CompletionProofRelaxationResolution {
    if !matches!(
        snapshot.resolution,
        CompletionProofRelaxationResolution::Clear {
            decision: CompletionProofRelaxationDecision::AllowWithoutCurrentProof,
            ..
        }
    ) {
        return snapshot.resolution.clone();
    }
    let Some(expected_head) = snapshot.head_oid.as_ref() else {
        return CompletionProofRelaxationResolution::Invalid {
            message: "repository completion-proof relaxation had no captured HEAD identity"
                .to_string(),
        };
    };
    let current_head = match git_text(repository_root, &["rev-parse", "--verify", "HEAD"]).await {
        Ok(value) => value,
        Err(message) => {
            return CompletionProofRelaxationResolution::Invalid { message };
        }
    };
    if current_head != *expected_head {
        return CompletionProofRelaxationResolution::Invalid {
            message: "repository completion-proof relaxation became stale after HEAD changed"
                .to_string(),
        };
    }
    let Some(changed_paths) = changed_workspace_paths(repository_root).await else {
        return CompletionProofRelaxationResolution::Invalid {
            message: "repository completion-proof instruction paths could not be verified"
                .to_string(),
        };
    };
    if changed_paths
        .iter()
        .any(|path| repository_instruction_filename(path).is_some())
        || snapshot
            .instruction_paths
            .iter()
            .any(|path| !repository_root.join(path).is_file())
    {
        return CompletionProofRelaxationResolution::Invalid {
            message: "repository completion-proof instructions changed after their provenance was captured"
                .to_string(),
        };
    }
    snapshot.resolution.clone()
}

fn effective_completion_proof_relaxation(
    current_user: Option<&PersistedCurrentUserRelaxation>,
    session_lineage_id: Option<&str>,
    repository: CompletionProofRelaxationResolution,
) -> EffectiveCompletionProofRelaxation {
    if let (Some(current_user), Some(session_lineage_id)) = (current_user, session_lineage_id)
        && current_user.session_lineage_id == session_lineage_id
    {
        return match current_user.decision {
            Some(CompletionProofRelaxationDecision::AllowWithoutCurrentProof) => {
                EffectiveCompletionProofRelaxation::Allowed(AppliedCompletionProofRelaxation {
                    authority: "current_user".to_string(),
                    binding: session_lineage_id.to_string(),
                    evidence: current_user.evidence.clone(),
                })
            }
            Some(CompletionProofRelaxationDecision::RequireCurrentProof) => {
                EffectiveCompletionProofRelaxation::NotAllowed {
                    reason: Some(
                        "The current user explicitly required current completion proof."
                            .to_string(),
                    ),
                }
            }
            None => EffectiveCompletionProofRelaxation::NotAllowed {
                reason: Some(
                    "The current-user completion-proof instruction was ambiguous, so it was rejected."
                        .to_string(),
                ),
            },
        };
    }

    match repository {
        CompletionProofRelaxationResolution::Clear {
            decision: CompletionProofRelaxationDecision::AllowWithoutCurrentProof,
            evidence,
        } => EffectiveCompletionProofRelaxation::Allowed(AppliedCompletionProofRelaxation {
            authority: "repository".to_string(),
            binding: evidence
                .first()
                .map(|evidence| evidence.provenance.clone())
                .unwrap_or_default(),
            evidence,
        }),
        CompletionProofRelaxationResolution::Clear {
            decision: CompletionProofRelaxationDecision::RequireCurrentProof,
            ..
        } => EffectiveCompletionProofRelaxation::NotAllowed {
            reason: Some(
                "The repository explicitly required current completion proof.".to_string(),
            ),
        },
        CompletionProofRelaxationResolution::Ambiguous { .. } => {
            EffectiveCompletionProofRelaxation::NotAllowed {
                reason: Some(
                    "Repository completion-proof instructions were ambiguous, so they were rejected."
                        .to_string(),
                ),
            }
        }
        CompletionProofRelaxationResolution::Invalid { message } => {
            EffectiveCompletionProofRelaxation::NotAllowed {
                reason: Some(format!(
                    "Repository completion-proof relaxation was rejected: {message}."
                )),
            }
        }
        CompletionProofRelaxationResolution::None => {
            EffectiveCompletionProofRelaxation::NotAllowed { reason: None }
        }
    }
}

fn repository_instruction_filename(path: &str) -> Option<&str> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    REPOSITORY_INSTRUCTION_FILENAMES
        .iter()
        .find(|candidate| {
            if cfg!(windows) {
                candidate.eq_ignore_ascii_case(filename)
            } else {
                **candidate == filename
            }
        })
        .copied()
}

fn is_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn git_text(repository_root: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = git_bytes(repository_root, args).await?;
    let value = String::from_utf8(bytes)
        .map_err(|_| "repository instruction provenance was not UTF-8 text".to_string())?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

async fn git_bytes(repository_root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository_root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn())
        .map_err(|error| format!("could not inspect repository instruction provenance: {error}"))?;
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("could not inspect repository instruction provenance: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not inspect repository instruction provenance with git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[derive(Clone, Debug)]
struct RepositoryCompletionProofConfig {
    policy_id: String,
    canonical_command: String,
    focused_command: String,
    documentation_command: Option<String>,
    frozen_inventory_hash: String,
    validation_ids: BTreeSet<String>,
    validation_evidence_contracts: BTreeMap<String, ValidationEvidenceContract>,
    validation_path_patterns: BTreeMap<String, ValidationInputContract>,
    baseline_exception_rules: BTreeSet<BaselineExceptionRule>,
    trusted_bundle_paths: BTreeSet<String>,
    trusted_runner_entrypoint_paths: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TrustedRunnerEntrypoint {
    relative_path: String,
    sha256: String,
}

#[derive(Clone, Debug)]
struct CompletionProofAuthority {
    config: RepositoryCompletionProofConfig,
    policy_runner_bundle_sha256: String,
    trusted_runner_entrypoints: BTreeSet<TrustedRunnerEntrypoint>,
}

async fn repository_requires_compiled_kd4_authority(repository_root: &Path) -> bool {
    // A KD4 checkout must not be able to downgrade its compiled policy merely by editing the
    // mutable policy file. Use several independent source-tree markers, then retain that decision
    // for the lifetime of the authenticated ledger.
    let distinctive_markers = [
        "SOURCEMAP.md",
        "codex-rs/.config/kd4-rust-tests.toml",
        "codex-rs/core/src/completion_proof.rs",
        "scripts/completion_proof.py",
    ];
    let mut marker_count = 0_u8;
    for relative_path in distinctive_markers {
        if tokio::fs::symlink_metadata(repository_root.join(relative_path))
            .await
            .is_ok()
        {
            marker_count = marker_count.saturating_add(1);
        }
    }
    if marker_count >= 2 {
        return true;
    }

    load_repository_config(repository_root)
        .await
        .is_ok_and(|config| config.policy_id == KD4_POLICY_ID)
}

fn trusted_bundle_sha256(members: &[TrustedBundleMember]) -> String {
    let mut sorted = members.to_vec();
    sorted.sort_by_key(|member| member.relative_path.replace('\\', "/"));
    let mut digest = Sha256::new();
    digest.update(b"KD4_COMPLETION_PROOF_BUNDLE_V1\0");
    for member in sorted {
        let relative_path = member.relative_path.replace('\\', "/");
        digest.update((relative_path.len() as u64).to_be_bytes());
        digest.update(relative_path.as_bytes());
        digest.update((member.bytes.len() as u64).to_be_bytes());
        digest.update(member.bytes);
    }
    format!("{:x}", digest.finalize())
}

fn compiled_kd4_authority() -> Result<CompletionProofAuthority, String> {
    let config_member = KD4_TRUSTED_BUNDLE_MEMBERS
        .iter()
        .find(|member| member.relative_path == CONFIG_RELATIVE_PATH)
        .ok_or_else(|| "the compiled KD4 policy bundle omitted its configuration".to_string())?;
    let config =
        parse_repository_config_bytes(config_member.bytes, Path::new(CONFIG_RELATIVE_PATH))?;
    if config.policy_id != KD4_POLICY_ID
        || config.frozen_inventory_hash != KD4_FROZEN_INVENTORY_HASH
    {
        return Err(
            "the compiled KD4 policy did not preserve its immutable policy and inventory identity"
                .to_string(),
        );
    }
    let runner_member = KD4_TRUSTED_BUNDLE_MEMBERS
        .iter()
        .find(|member| member.relative_path == "scripts/completion_proof.py")
        .ok_or_else(|| {
            "the compiled KD4 policy bundle omitted its runner entrypoint".to_string()
        })?;
    Ok(CompletionProofAuthority {
        config,
        policy_runner_bundle_sha256: trusted_bundle_sha256(KD4_TRUSTED_BUNDLE_MEMBERS),
        trusted_runner_entrypoints: BTreeSet::from([TrustedRunnerEntrypoint {
            relative_path: runner_member.relative_path.to_string(),
            sha256: format!("{:x}", Sha256::digest(runner_member.bytes)),
        }]),
    })
}

async fn verify_live_kd4_bundle(repository_root: &Path) -> Result<(), String> {
    for member in KD4_TRUSTED_BUNDLE_MEMBERS {
        let path = repository_root.join(member.relative_path);
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
            format!(
                "could not inspect trusted KD4 bundle member {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "trusted KD4 bundle member {} is not a regular non-symlink file",
                path.display()
            ));
        }
        let live_bytes = tokio::fs::read(&path).await.map_err(|error| {
            format!(
                "could not read trusted KD4 bundle member {}: {error}",
                path.display()
            )
        })?;
        if live_bytes.as_slice() != member.bytes {
            return Err(format!(
                "trusted KD4 bundle member {} differs from the policy compiled into this runtime; rebuild and reactivate KD4 before certification",
                path.display()
            ));
        }
    }
    Ok(())
}

async fn load_completion_proof_authority(
    repository_root: &Path,
    requires_compiled_kd4_authority: bool,
) -> Result<CompletionProofAuthority, String> {
    if requires_compiled_kd4_authority {
        let authority = compiled_kd4_authority()?;
        verify_live_kd4_bundle(repository_root).await?;
        return Ok(authority);
    }

    let path = repository_root.join(CONFIG_RELATIVE_PATH);
    let initially_observed_bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let initially_observed_config =
        parse_repository_config_bytes(&initially_observed_bytes, &path)?;
    if initially_observed_config.policy_id == KD4_POLICY_ID {
        return Err(
            "a KD4 policy may be authorized only by the policy compiled into the active runtime"
                .to_string(),
        );
    }

    let mut trusted_paths = initially_observed_config.trusted_bundle_paths.clone();
    trusted_paths.insert(CONFIG_RELATIVE_PATH.to_string());
    let mut trusted_members = BTreeMap::new();
    for relative_path in trusted_paths {
        let bytes = load_clean_head_trusted_member(repository_root, &relative_path).await?;
        trusted_members.insert(relative_path, bytes);
    }
    if trusted_members.get(CONFIG_RELATIVE_PATH).map(Vec::as_slice)
        != Some(initially_observed_bytes.as_slice())
    {
        return Err(
            "the completion-proof configuration changed while its committed trust bundle was being loaded"
                .to_string(),
        );
    }
    let config = parse_repository_config_bytes(
        trusted_members
            .get(CONFIG_RELATIVE_PATH)
            .ok_or_else(|| "the repository trust bundle omitted its policy".to_string())?,
        &path,
    )?;
    let trusted_runner_entrypoints = config
        .trusted_runner_entrypoint_paths
        .iter()
        .map(|relative_path| {
            let bytes = trusted_members.get(relative_path).ok_or_else(|| {
                format!("trusted runner entrypoint {relative_path} was not in the policy bundle")
            })?;
            Ok(TrustedRunnerEntrypoint {
                relative_path: relative_path.clone(),
                sha256: format!("{:x}", Sha256::digest(bytes)),
            })
        })
        .collect::<Result<BTreeSet<_>, String>>()?;
    Ok(CompletionProofAuthority {
        config,
        policy_runner_bundle_sha256: trusted_repository_bundle_sha256(&trusted_members),
        trusted_runner_entrypoints,
    })
}

fn trusted_repository_bundle_sha256(members: &BTreeMap<String, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"REPOSITORY_COMPLETION_PROOF_POLICY_V2\0");
    for (relative_path, bytes) in members {
        digest.update((relative_path.len() as u64).to_be_bytes());
        digest.update(relative_path.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

async fn load_clean_head_trusted_member(
    repository_root: &Path,
    relative_path: &str,
) -> Result<Vec<u8>, String> {
    let relative_path = normalize_trusted_repository_path(relative_path)?;
    let path = repository_root.join(&relative_path);
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| {
        format!(
            "could not inspect trusted repository policy member {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "trusted repository policy member {} is not a regular non-symlink file",
            path.display()
        ));
    }

    let tree = git_bytes(
        repository_root,
        &["ls-tree", "-z", "--full-tree", "HEAD", "--", &relative_path],
    )
    .await?;
    let expected_suffix = format!("\t{relative_path}\0");
    let tree_text = String::from_utf8(tree).map_err(|_| {
        format!("trusted repository policy member {relative_path} had a non-UTF-8 Git identity")
    })?;
    if !(tree_text.starts_with("100644 blob ") || tree_text.starts_with("100755 blob "))
        || !tree_text.ends_with(&expected_suffix)
        || tree_text.matches('\0').count() != 1
    {
        return Err(format!(
            "trusted repository policy member {relative_path} is not one exact regular file committed at HEAD"
        ));
    }

    let before = tokio::fs::read(&path).await.map_err(|error| {
        format!(
            "could not read trusted repository policy member {}: {error}",
            path.display()
        )
    })?;
    let mut command = tokio::process::Command::new("git");
    command
        .args(["diff", "--quiet", "HEAD", "--", &relative_path])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository_root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child =
        codex_utils_pty::with_windows_child_creation(|_| command.spawn()).map_err(|error| {
            format!("could not verify committed trusted policy member {relative_path}: {error}")
        })?;
    let diff = child.wait_with_output().await.map_err(|error| {
        format!("could not verify committed trusted policy member {relative_path}: {error}")
    })?;
    if !diff.status.success() {
        return Err(if diff.status.code() == Some(1) {
            format!(
                "trusted repository policy member {relative_path} differs from the version explicitly trusted at HEAD"
            )
        } else {
            format!(
                "could not verify trusted repository policy member {relative_path}: {}",
                String::from_utf8_lossy(&diff.stderr).trim()
            )
        });
    }
    let after = tokio::fs::read(&path).await.map_err(|error| {
        format!(
            "could not reread trusted repository policy member {}: {error}",
            path.display()
        )
    })?;
    if before != after {
        return Err(format!(
            "trusted repository policy member {relative_path} changed while its authority was being established"
        ));
    }
    Ok(after)
}

async fn load_repository_config(
    repository_root: &Path,
) -> Result<RepositoryCompletionProofConfig, String> {
    let path = repository_root.join(CONFIG_RELATIVE_PATH);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    parse_repository_config_bytes(&bytes, &path)
}

fn parse_repository_config_bytes(
    bytes: &[u8],
    path: &Path,
) -> Result<RepositoryCompletionProofConfig, String> {
    let contents =
        std::str::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    let value = toml::from_str::<toml::Value>(contents)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("{} must contain a TOML table", path.display()))?;
    let allowed_top_level_fields = BTreeSet::from([
        "schema_version",
        "policy_id",
        "canonical_command",
        "command",
        "focused_command",
        "documentation_command",
        "frozen_inventory_hash",
        "repository_root",
        "frozen_inventory",
        "replacement_ledger",
        "host_platform",
        "trusted_bundle_paths",
        "trusted_runner_entrypoints",
        "baseline_exception",
        "validation",
    ]);
    if let Some(unknown) = table
        .keys()
        .find(|key| !allowed_top_level_fields.contains(key.as_str()))
    {
        return Err(format!(
            "completion-proof configuration contains unknown top-level field {unknown}"
        ));
    }
    if table.contains_key("canonical_command") && table.contains_key("command") {
        return Err(
            "completion-proof configuration cannot declare both canonical_command and command"
                .to_string(),
        );
    }
    let canonical_command = value
        .get("canonical_command")
        .or_else(|| value.get("command"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("{} does not declare canonical_command", path.display()))?
        .to_string();
    if canonical_command.trim() != canonical_command || canonical_command.is_empty() {
        return Err("canonical completion-proof command must be exact and nonempty".to_string());
    }
    let policy_id = exact_nonempty_config_string(&value, "policy_id")?;
    let frozen_inventory_hash = exact_nonempty_config_string(&value, "frozen_inventory_hash")?;
    if !is_sha256(&frozen_inventory_hash) {
        return Err("completion-proof frozen_inventory_hash must be lowercase SHA-256".to_string());
    }
    if policy_id == KD4_POLICY_ID && frozen_inventory_hash != KD4_FROZEN_INVENTORY_HASH {
        return Err(
            "KD4 completion-proof configuration did not preserve the compiled frozen inventory hash"
                .to_string(),
        );
    }
    let focused_command = exact_nonempty_config_string(&value, "focused_command")?;
    if focused_command.matches("{validation_id}").count() != 1 {
        return Err(
            "focused validation command must contain exactly one {validation_id} placeholder"
                .to_string(),
        );
    }
    let documentation_command = value
        .get("documentation_command")
        .map(|value| {
            value
                .as_str()
                .filter(|command| !command.is_empty() && command.trim() == *command)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    "documentation validation command must be an exact nonempty string".to_string()
                })
        })
        .transpose()?;
    if documentation_command.as_deref() == Some(canonical_command.as_str()) {
        return Err(
            "documentation validation command must remain separate from canonical certification"
                .to_string(),
        );
    }
    if focused_command == canonical_command
        || documentation_command.as_deref() == Some(focused_command.as_str())
    {
        return Err(
            "focused validation, documentation validation, and canonical certification commands must remain distinct"
                .to_string(),
        );
    }
    if value
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        != Some(2)
    {
        return Err("completion-proof configuration schema_version must be 2".to_string());
    }
    let (validation_ids, validation_evidence_contracts, validation_path_patterns) =
        extract_validations(&value)?;
    if validation_ids.is_empty() {
        return Err("completion-proof configuration declares no required validations".to_string());
    }
    let baseline_exception_rules = extract_baseline_exception_rules(&value)?;
    let trusted_bundle_paths = extract_trusted_repository_paths(&value, "trusted_bundle_paths")?;
    let trusted_runner_entrypoint_paths =
        extract_trusted_repository_paths(&value, "trusted_runner_entrypoints")?;
    if policy_id != KD4_POLICY_ID {
        if trusted_bundle_paths.is_empty() || trusted_runner_entrypoint_paths.is_empty() {
            return Err(
                "a non-KD4 completion-proof policy must explicitly declare nonempty trusted_bundle_paths and trusted_runner_entrypoints"
                    .to_string(),
            );
        }
        if !trusted_runner_entrypoint_paths
            .iter()
            .all(|path| trusted_bundle_paths.contains(path))
        {
            return Err(
                "every trusted_runner_entrypoints path must also be present in trusted_bundle_paths"
                    .to_string(),
            );
        }
    }
    Ok(RepositoryCompletionProofConfig {
        policy_id,
        canonical_command,
        focused_command,
        documentation_command,
        frozen_inventory_hash,
        validation_ids,
        validation_evidence_contracts,
        validation_path_patterns,
        baseline_exception_rules,
        trusted_bundle_paths,
        trusted_runner_entrypoint_paths,
    })
}

fn extract_trusted_repository_paths(
    value: &toml::Value,
    key: &str,
) -> Result<BTreeSet<String>, String> {
    let Some(values) = value.get(key) else {
        return Ok(BTreeSet::new());
    };
    let values = values
        .as_array()
        .ok_or_else(|| format!("completion-proof {key} must be an array of repository paths"))?;
    let mut paths = BTreeSet::new();
    for value in values {
        let path = value
            .as_str()
            .filter(|path| !path.is_empty() && path.trim() == *path)
            .ok_or_else(|| format!("completion-proof {key} contains an invalid path"))?;
        let normalized = normalize_trusted_repository_path(path)?;
        if !paths.insert(normalized) {
            return Err(format!("completion-proof {key} contains a duplicate path"));
        }
    }
    Ok(paths)
}

fn normalize_trusted_repository_path(path: &str) -> Result<String, String> {
    if path.contains('\\') || path.starts_with('/') || path.ends_with('/') {
        return Err(format!(
            "trusted completion-proof path {path:?} must be a normalized repository-relative path"
        ));
    }
    let parsed = Path::new(path);
    let components = parsed.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || components
            .first()
            .is_some_and(|component| component.as_os_str() == ".git")
    {
        return Err(format!(
            "trusted completion-proof path {path:?} escapes or names repository metadata"
        ));
    }
    let normalized = components
        .iter()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if normalized != path {
        return Err(format!(
            "trusted completion-proof path {path:?} is not normalized"
        ));
    }
    Ok(normalized)
}

fn exact_nonempty_config_string(value: &toml::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty() && value.trim() == *value)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("completion-proof configuration requires exact nonempty {key}"))
}

fn extract_baseline_exception_rules(
    value: &toml::Value,
) -> Result<BTreeSet<BaselineExceptionRule>, String> {
    let Some(entries) = value.get("baseline_exception") else {
        return Ok(BTreeSet::new());
    };
    let entries = entries
        .as_array()
        .ok_or_else(|| "completion-proof baseline_exception entries must be tables".to_string())?;
    let mut rules = BTreeSet::new();
    for entry in entries {
        let table = entry.as_table().ok_or_else(|| {
            "each completion-proof baseline_exception must be a table".to_string()
        })?;
        let allowed = BTreeSet::from(["id_prefix", "source_prefix", "kind", "source", "text"]);
        if let Some(unknown) = table.keys().find(|key| !allowed.contains(key.as_str())) {
            return Err(format!(
                "completion-proof baseline_exception contains unknown field {unknown}"
            ));
        }
        let exact_optional = |key: &str| -> Result<Option<String>, String> {
            table
                .get(key)
                .map(|value| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty() && value.trim() == *value)
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| {
                            format!(
                                "completion-proof baseline_exception {key} must be exact and nonempty"
                            )
                        })
                })
                .transpose()
        };
        let id_prefix = exact_optional("id_prefix")?;
        let source_prefix = exact_optional("source_prefix")?;
        if id_prefix.is_some() == source_prefix.is_some() {
            return Err(
                "each baseline_exception must set exactly one of id_prefix or source_prefix"
                    .to_string(),
            );
        }
        let exact_required = |key: &str| -> Result<String, String> {
            exact_optional(key)?
                .ok_or_else(|| format!("completion-proof baseline_exception requires {key}"))
        };
        let kind = exact_required("kind")?;
        if !is_baseline_exception_kind(&kind) {
            return Err(format!(
                "completion-proof baseline_exception has unsupported kind {kind}"
            ));
        }
        let rule = BaselineExceptionRule {
            id_prefix,
            source_prefix,
            kind,
            source: exact_required("source")?,
            text: exact_required("text")?,
        };
        if !rules.insert(rule) {
            return Err("completion-proof baseline_exception is duplicated".to_string());
        }
    }
    Ok(rules)
}

fn extract_validations(
    value: &toml::Value,
) -> Result<
    (
        BTreeSet<String>,
        BTreeMap<String, ValidationEvidenceContract>,
        BTreeMap<String, ValidationInputContract>,
    ),
    String,
> {
    let mut ids = BTreeSet::new();
    let mut rust_gates = BTreeSet::new();
    let mut evidence_contracts = BTreeMap::new();
    let mut path_patterns = BTreeMap::new();
    let entries = value
        .get("validation")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| {
            "completion-proof configuration has no [[validation]] entries".to_string()
        })?;
    for entry in entries {
        let table = entry
            .as_table()
            .ok_or_else(|| "each completion-proof validation must be a table".to_string())?;
        let id = table
            .get("id")
            .and_then(toml::Value::as_str)
            .filter(|id| !id.trim().is_empty() && id.trim() == *id)
            .ok_or_else(|| {
                "each completion-proof validation requires an exact nonempty id".to_string()
            })?;
        if !is_validation_id(id) {
            return Err(format!(
                "completion-proof validation id {id} must use only letters, digits, dot, underscore, or hyphen and start with a letter or digit"
            ));
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("completion-proof validation id {id} is duplicated"));
        }
        let runner = table
            .get("runner")
            .and_then(toml::Value::as_str)
            .filter(|runner| !runner.is_empty() && runner.trim() == *runner)
            .ok_or_else(|| {
                format!("completion-proof validation {id} requires an exact nonempty runner")
            })?;
        let mut allowed_fields = BTreeSet::from([
            "id",
            "runner",
            "owned_paths",
            "consumed_paths",
            "path_set_paths",
            "evidence_path_manifests",
            "timeout_seconds",
        ]);
        match runner {
            "typed-validation" => {
                allowed_fields.insert("validation_type");
            }
            "rust-gate" => {
                allowed_fields.insert("gate");
            }
            _ => {}
        }
        if let Some(unknown) = table
            .keys()
            .find(|key| !allowed_fields.contains(key.as_str()))
        {
            return Err(format!(
                "completion-proof validation {id} contains unknown field {unknown}"
            ));
        }
        let declared_validation_type = table
            .get("validation_type")
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty() && value.trim() == *value)
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        format!("completion-proof validation {id} has an invalid validation_type")
                    })
            })
            .transpose()?;
        let evidence_contract = match runner {
            "inventory-reconciliation" => ValidationEvidenceContract {
                runner: runner.to_string(),
                runner_selector: None,
                evidence_kind: ValidationEvidenceKind::InventoryReconciliation,
                validation_type: None,
                input_contract_digest: None,
            },
            "rust-nextest"
            | "rust-doctest"
            | "python-unittest"
            | "python-pytest"
            | "javascript-jest"
            | "argument-comment-lint-native"
            | "windows-sandbox-smoke"
            | "rust-gate" => ValidationEvidenceContract {
                runner: runner.to_string(),
                runner_selector: None,
                evidence_kind: ValidationEvidenceKind::StructuredTest,
                validation_type: None,
                input_contract_digest: None,
            },
            "typed-validation" => ValidationEvidenceContract {
                runner: runner.to_string(),
                runner_selector: None,
                evidence_kind: ValidationEvidenceKind::TypedNonTest,
                validation_type: Some(declared_validation_type.clone().ok_or_else(|| {
                    format!("typed completion-proof validation {id} requires validation_type")
                })?),
                input_contract_digest: None,
            },
            other => {
                return Err(format!(
                    "completion-proof validation {id} uses unsupported runner {other}"
                ));
            }
        };
        if runner != "typed-validation" && declared_validation_type.is_some() {
            return Err(format!(
                "completion-proof validation {id} may declare validation_type only for typed-validation"
            ));
        }
        let mut evidence_contract = evidence_contract;
        if runner == "rust-gate" {
            let gate = table
                .get("gate")
                .and_then(toml::Value::as_str)
                .filter(|gate| !gate.is_empty() && gate.trim() == *gate && is_validation_id(gate))
                .ok_or_else(|| {
                    format!("completion-proof validation {id} requires an exact valid gate")
                })?;
            if !rust_gates.insert(gate.to_string()) {
                return Err(format!(
                    "completion-proof Rust gate {gate} is configured more than once"
                ));
            }
            evidence_contract.runner_selector = Some(gate.to_string());
        }
        if table
            .get("timeout_seconds")
            .and_then(toml::Value::as_integer)
            .is_none_or(|timeout| timeout <= 0)
        {
            return Err(format!(
                "completion-proof validation {id} requires a positive timeout_seconds"
            ));
        }
        let mut content_paths = BTreeSet::new();
        for key in ["owned_paths", "consumed_paths"] {
            let patterns = table
                .get(key)
                .and_then(toml::Value::as_array)
                .ok_or_else(|| format!("completion-proof validation {id} has no {key}"))?;
            if patterns.is_empty() {
                return Err(format!("completion-proof validation {id} has empty {key}"));
            }
            for pattern in patterns {
                let pattern = pattern
                    .as_str()
                    .filter(|pattern| !pattern.trim().is_empty() && pattern.trim() == *pattern)
                    .ok_or_else(|| {
                        format!("completion-proof validation {id} has an invalid {key} entry")
                    })?;
                let normalized = pattern.replace('\\', "/");
                glob::Pattern::new(&normalized).map_err(|error| {
                    format!("completion-proof validation {id} has invalid path pattern {pattern}: {error}")
                })?;
                content_paths.insert(normalized);
            }
        }
        let optional_patterns = |key: &str| -> Result<BTreeSet<String>, String> {
            let Some(patterns) = table.get(key) else {
                return Ok(BTreeSet::new());
            };
            let patterns = patterns
                .as_array()
                .ok_or_else(|| format!("completion-proof validation {id} has invalid {key}"))?;
            let mut normalized_patterns = BTreeSet::new();
            for pattern in patterns {
                let pattern = pattern
                    .as_str()
                    .filter(|pattern| !pattern.trim().is_empty() && pattern.trim() == *pattern)
                    .ok_or_else(|| {
                        format!("completion-proof validation {id} has an invalid {key} entry")
                    })?;
                let normalized = pattern.replace('\\', "/");
                glob::Pattern::new(&normalized).map_err(|error| {
                    format!("completion-proof validation {id} has invalid path pattern {pattern}: {error}")
                })?;
                normalized_patterns.insert(normalized);
            }
            Ok(normalized_patterns)
        };
        let path_set_paths = optional_patterns("path_set_paths")?;
        let evidence_path_manifests = optional_patterns("evidence_path_manifests")?;
        if evidence_path_manifests.iter().any(|path| {
            path.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
                || !safe_repository_relative_path(Path::new(path))
        }) {
            return Err(format!(
                "completion-proof validation {id} evidence_path_manifests must contain exact repository-relative paths"
            ));
        }
        let input_contract = ValidationInputContract {
            schema_version: 1,
            content_paths,
            path_set_paths,
            evidence_path_manifests,
        };
        let mut evidence_contract = evidence_contract;
        if matches!(
            evidence_contract.evidence_kind,
            ValidationEvidenceKind::TypedNonTest | ValidationEvidenceKind::InventoryReconciliation
        ) {
            evidence_contract.input_contract_digest = Some(input_contract.digest()?);
        }
        evidence_contracts.insert(id.to_string(), evidence_contract);
        path_patterns.insert(id.to_string(), input_contract);
    }
    Ok((ids, evidence_contracts, path_patterns))
}

fn is_validation_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

async fn load_inventory_hash(
    repository_root: &Path,
    config: &RepositoryCompletionProofConfig,
) -> Result<String, String> {
    let path = repository_root.join(INVENTORY_RELATIVE_PATH);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let inventory: FrozenTestInventoryV1 = serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    if inventory.schema_version != 1 || inventory.tests.is_empty() {
        return Err(format!(
            "{} is not a nonempty frozen inventory schema version 1",
            path.display()
        ));
    }
    if !is_sha256(&inventory.inventory_hash) {
        return Err(format!("{} has no valid inventory_hash", path.display()));
    }
    for (field, value) in [
        ("baseline_commit", inventory.baseline_commit.as_deref()),
        (
            "baseline_workspace_fingerprint",
            inventory.baseline_workspace_fingerprint.as_deref(),
        ),
        ("host_platform", inventory.host_platform.as_deref()),
    ] {
        if value.is_some_and(|value| value.is_empty() || value.trim() != value) {
            return Err(format!(
                "{} has an inexact or empty {field}",
                path.display()
            ));
        }
    }
    if inventory
        .baseline_workspace_fingerprint
        .as_deref()
        .is_some_and(|value| !is_sha256(value))
    {
        return Err(format!(
            "{} has an invalid baseline_workspace_fingerprint",
            path.display()
        ));
    }
    if config.policy_id == KD4_POLICY_ID
        && (inventory.baseline_commit.is_none()
            || inventory.baseline_workspace_fingerprint.is_none()
            || inventory.host_platform.is_none())
    {
        return Err(format!(
            "{} omitted required KD4 frozen-baseline metadata",
            path.display()
        ));
    }

    let mut baseline_ids = BTreeSet::new();
    let mut normalized_rows = Vec::with_capacity(inventory.tests.len());
    for row in inventory.tests {
        if row.baseline_id.trim().is_empty()
            || row.framework.trim().is_empty()
            || row.native_id.trim().is_empty()
            || row.source.trim().is_empty()
            || !baseline_ids.insert(row.baseline_id.clone())
        {
            return Err(format!(
                "{} contains an empty or duplicate frozen test identity",
                path.display()
            ));
        }
        let mut platforms = row.platforms;
        platforms.sort();
        let mut normalized = BTreeMap::new();
        normalized.insert(
            "baseline_id".to_string(),
            serde_json::Value::String(row.baseline_id),
        );
        normalized.insert(
            "framework".to_string(),
            serde_json::Value::String(row.framework),
        );
        normalized.insert("ignored".to_string(), serde_json::Value::Bool(row.ignored));
        normalized.insert(
            "native_id".to_string(),
            serde_json::Value::String(row.native_id),
        );
        normalized.insert(
            "platforms".to_string(),
            serde_json::Value::Array(
                platforms
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        normalized.insert("source".to_string(), serde_json::Value::String(row.source));
        normalized_rows.push(normalized);
    }
    normalized_rows.sort_by(|left, right| {
        left.get("baseline_id")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("baseline_id").and_then(serde_json::Value::as_str))
    });
    let canonical = BTreeMap::from([
        (
            "schema_version".to_string(),
            serde_json::Value::Number(1_u64.into()),
        ),
        (
            "tests".to_string(),
            serde_json::to_value(normalized_rows)
                .map_err(|error| format!("could not normalize {}: {error}", path.display()))?,
        ),
    ]);
    let canonical_bytes = serde_json::to_vec(&canonical)
        .map_err(|error| format!("could not normalize {}: {error}", path.display()))?;
    let computed_hash = format!("{:x}", Sha256::digest(canonical_bytes));
    if inventory.inventory_hash != computed_hash {
        return Err(format!(
            "{} inventory_hash does not match its frozen rows",
            path.display()
        ));
    }
    if computed_hash != config.frozen_inventory_hash {
        return Err(format!(
            "{} does not match the immutable inventory hash configured for policy {}",
            path.display(),
            config.policy_id
        ));
    }
    Ok(computed_hash)
}

fn require_exact_json_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
    context: &str,
) -> Result<(), String> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        let unknown = actual.difference(&expected).copied().collect::<Vec<_>>();
        return Err(format!(
            "{context} fields did not match the schema; missing={missing:?} unknown={unknown:?}"
        ));
    }
    Ok(())
}

fn exact_json_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.trim() == *value)
        .ok_or_else(|| format!("{context} requires exact nonempty {key}"))
}

fn active_inventory_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn validate_baseline_exception_semantics(
    baseline_id: &str,
    kind: &str,
    frozen_row: &FrozenTestInventoryRowV1,
) -> Result<(), String> {
    let context = format!("{baseline_id}: {kind} exception frozen inventory row");
    match kind {
        "live-service" => {
            if !frozen_row.ignored {
                return Err(format!("{context} must have ignored=true"));
            }
        }
        "off-host" | "platform-pending" => {
            if frozen_row.platforms.is_empty()
                || frozen_row
                    .platforms
                    .iter()
                    .any(|platform| platform.is_empty() || platform.trim() != platform)
            {
                return Err(format!("{context} must have exact nonempty platform names"));
            }
            let active_platform = active_inventory_platform();
            if frozen_row
                .platforms
                .iter()
                .any(|platform| platform.eq_ignore_ascii_case(active_platform))
            {
                return Err(format!(
                    "{context} includes active platform {active_platform:?}"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

async fn load_expected_provenance(
    repository_root: &Path,
    config: &RepositoryCompletionProofConfig,
    expected_inventory_hash: &str,
) -> Result<
    (
        BTreeSet<BaselineExceptionEvidence>,
        BTreeSet<CompletionOverrideEvidence>,
    ),
    String,
> {
    let inventory_path = repository_root.join(INVENTORY_RELATIVE_PATH);
    let inventory_bytes = tokio::fs::read(&inventory_path)
        .await
        .map_err(|error| format!("could not read {}: {error}", inventory_path.display()))?;
    let inventory: FrozenTestInventoryV1 = serde_json::from_slice(&inventory_bytes)
        .map_err(|error| format!("could not parse {}: {error}", inventory_path.display()))?;
    if inventory.inventory_hash != expected_inventory_hash {
        return Err("the frozen inventory changed while certification was prepared".to_string());
    }
    let frozen_rows = inventory
        .tests
        .into_iter()
        .map(|row| (row.baseline_id.clone(), row))
        .collect::<BTreeMap<_, _>>();

    let ledger_path = repository_root.join(REPLACEMENT_LEDGER_RELATIVE_PATH);
    let ledger_bytes = tokio::fs::read(&ledger_path)
        .await
        .map_err(|error| format!("could not read {}: {error}", ledger_path.display()))?;
    let ledger: serde_json::Value = serde_json::from_slice(&ledger_bytes)
        .map_err(|error| format!("could not parse {}: {error}", ledger_path.display()))?;
    let ledger = ledger
        .as_object()
        .ok_or_else(|| format!("{} must contain a JSON object", ledger_path.display()))?;
    let allowed_ledger_fields = BTreeSet::from([
        "schema_version",
        "frozen_inventory_hash",
        "rows",
        "additions",
        "overrides",
    ]);
    if let Some(unknown) = ledger
        .keys()
        .find(|key| !allowed_ledger_fields.contains(key.as_str()))
    {
        return Err(format!(
            "{} contains unknown field {unknown}",
            ledger_path.display()
        ));
    }
    if ledger
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || ledger
            .get("frozen_inventory_hash")
            .and_then(serde_json::Value::as_str)
            != Some(expected_inventory_hash)
    {
        return Err(
            "the replacement ledger schema or frozen inventory hash did not match".to_string(),
        );
    }

    let rows = ledger
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the replacement ledger has no rows array".to_string())?;
    let mut exceptions = BTreeSet::new();
    let mut seen_baseline_ids = BTreeSet::new();
    for row in rows {
        let Some(row) = row.as_object() else {
            return Err("the replacement ledger contains a non-object row".to_string());
        };
        let baseline_id = exact_json_string(row, "baseline_id", "a replacement-ledger row")?;
        if !seen_baseline_ids.insert(baseline_id.to_string()) {
            return Err(format!(
                "replacement-ledger baseline row {baseline_id} is duplicated"
            ));
        }
        let frozen_row = frozen_rows.get(baseline_id).ok_or_else(|| {
            format!("replacement-ledger row {baseline_id} is not in the frozen inventory")
        })?;
        let resolution = exact_json_string(
            row,
            "resolution",
            &format!("replacement-ledger row {baseline_id}"),
        )?;
        match resolution {
            "unresolved" => require_exact_json_fields(
                row,
                &["baseline_id", "resolution"],
                &format!("replacement-ledger unresolved row {baseline_id}"),
            )?,
            "replacement" => {
                require_exact_json_fields(
                    row,
                    &[
                        "baseline_id",
                        "resolution",
                        "replacement_ids",
                        "preserved_behavior",
                        "product_path",
                        "validation_id",
                    ],
                    &format!("replacement-ledger replacement row {baseline_id}"),
                )?;
                let replacement_ids = row
                    .get("replacement_ids")
                    .and_then(serde_json::Value::as_array)
                    .filter(|ids| !ids.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "replacement-ledger replacement {baseline_id} has no replacement_ids"
                        )
                    })?;
                let mut unique_replacement_ids = BTreeSet::new();
                for replacement_id in replacement_ids {
                    let replacement_id = replacement_id
                        .as_str()
                        .filter(|value| !value.is_empty() && value.trim() == *value)
                        .ok_or_else(|| {
                            format!(
                                "replacement-ledger replacement {baseline_id} has an invalid replacement ID"
                            )
                        })?;
                    if replacement_id == baseline_id {
                        return Err(format!(
                            "replacement-ledger baseline test {baseline_id} cannot replace itself"
                        ));
                    }
                    if !unique_replacement_ids.insert(replacement_id) {
                        return Err(format!(
                            "replacement-ledger replacement {baseline_id} repeats replacement ID {replacement_id}"
                        ));
                    }
                }
                exact_json_string(
                    row,
                    "preserved_behavior",
                    &format!("replacement-ledger replacement {baseline_id}"),
                )?;
                exact_json_string(
                    row,
                    "product_path",
                    &format!("replacement-ledger replacement {baseline_id}"),
                )?;
                let validation_id = exact_json_string(
                    row,
                    "validation_id",
                    &format!("replacement-ledger replacement {baseline_id}"),
                )?;
                if !config.validation_ids.contains(validation_id) {
                    return Err(format!(
                        "replacement-ledger replacement {baseline_id} names unknown validation {validation_id}"
                    ));
                }
            }
            "exception" => {
                require_exact_json_fields(
                    row,
                    &["baseline_id", "resolution", "provenance"],
                    &format!("replacement-ledger exception {baseline_id}"),
                )?;
                let provenance = row.get("provenance").cloned().ok_or_else(|| {
                    format!("replacement-ledger exception {baseline_id} has no provenance")
                })?;
                let provenance: BaselineExceptionProvenance =
                    serde_json::from_value(provenance).map_err(|error| {
                        format!(
                            "replacement-ledger exception {baseline_id} has invalid provenance: {error}"
                        )
                    })?;
                if !is_baseline_exception_kind(&provenance.kind)
                    || provenance.source.is_empty()
                    || provenance.source.trim() != provenance.source
                    || provenance.text.is_empty()
                    || provenance.text.trim() != provenance.text
                {
                    return Err(format!(
                        "replacement-ledger exception {baseline_id} has unsupported or inexact provenance"
                    ));
                }
                validate_baseline_exception_semantics(baseline_id, &provenance.kind, frozen_row)?;
                let matching_rules = config
                    .baseline_exception_rules
                    .iter()
                    .filter(|rule| {
                        let selector_matches = rule
                            .id_prefix
                            .as_ref()
                            .is_some_and(|prefix| baseline_id.starts_with(prefix))
                            || rule
                                .source_prefix
                                .as_ref()
                                .is_some_and(|prefix| frozen_row.source.starts_with(prefix));
                        selector_matches
                            && rule.kind == provenance.kind
                            && rule.source == provenance.source
                            && rule.text == provenance.text
                    })
                    .count();
                if matching_rules != 1 {
                    return Err(format!(
                        "replacement-ledger exception {baseline_id} does not have exactly one matching configured provenance rule"
                    ));
                }
                let evidence = BaselineExceptionEvidence {
                    baseline_id: baseline_id.to_string(),
                    kind: provenance.kind,
                    source: provenance.source,
                    text: provenance.text,
                };
                if !exceptions.insert(evidence) {
                    return Err(format!(
                        "replacement-ledger exception {baseline_id} is duplicated"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "replacement-ledger row {baseline_id} has unknown resolution {other}"
                ));
            }
        }
    }
    let frozen_ids = frozen_rows.keys().cloned().collect::<BTreeSet<_>>();
    if seen_baseline_ids != frozen_ids {
        let missing = frozen_ids
            .difference(&seen_baseline_ids)
            .take(20)
            .cloned()
            .collect::<Vec<_>>();
        return Err(format!(
            "the replacement ledger does not exactly cover the frozen inventory; missing={missing:?}"
        ));
    }

    let addition_values: &[serde_json::Value] = match ledger.get("additions") {
        Some(value) => value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| "the replacement ledger additions value is not an array".to_string())?,
        None => &[],
    };
    let mut addition_ids = BTreeSet::new();
    for value in addition_values {
        let addition = value
            .as_object()
            .ok_or_else(|| "the replacement ledger contains a non-object addition".to_string())?;
        require_exact_json_fields(
            addition,
            &[
                "test_id",
                "preserved_behavior",
                "product_path",
                "validation_id",
                "provenance",
            ],
            "replacement-ledger addition",
        )?;
        let test_id = exact_json_string(addition, "test_id", "replacement-ledger addition")?;
        if frozen_rows.contains_key(test_id) {
            return Err(format!(
                "replacement-ledger addition {test_id} is a frozen baseline identity"
            ));
        }
        if !addition_ids.insert(test_id.to_string()) {
            return Err(format!(
                "replacement-ledger addition {test_id} is duplicated"
            ));
        }
        exact_json_string(
            addition,
            "preserved_behavior",
            &format!("replacement-ledger addition {test_id}"),
        )?;
        exact_json_string(
            addition,
            "product_path",
            &format!("replacement-ledger addition {test_id}"),
        )?;
        let validation_id = exact_json_string(
            addition,
            "validation_id",
            &format!("replacement-ledger addition {test_id}"),
        )?;
        if !config.validation_ids.contains(validation_id) {
            return Err(format!(
                "replacement-ledger addition {test_id} names unknown validation {validation_id}"
            ));
        }
        let provenance = addition
            .get("provenance")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                format!("replacement-ledger addition {test_id} has no provenance object")
            })?;
        require_exact_json_fields(
            provenance,
            &["kind", "source", "text"],
            &format!("replacement-ledger addition {test_id} provenance"),
        )?;
        if exact_json_string(
            provenance,
            "kind",
            &format!("replacement-ledger addition {test_id} provenance"),
        )? != "policy-addition"
        {
            return Err(format!(
                "replacement-ledger addition {test_id} provenance kind must be policy-addition"
            ));
        }
        exact_json_string(
            provenance,
            "source",
            &format!("replacement-ledger addition {test_id} provenance"),
        )?;
        exact_json_string(
            provenance,
            "text",
            &format!("replacement-ledger addition {test_id} provenance"),
        )?;
    }

    let override_values = ledger
        .get("overrides")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the replacement ledger has no overrides array".to_string())?;
    let mut overrides = BTreeSet::new();
    for value in override_values {
        let evidence: CompletionOverrideEvidence = serde_json::from_value(value.clone())
            .map_err(|error| format!("replacement-ledger override is invalid: {error}"))?;
        if evidence.text.is_empty()
            || evidence.text.trim() != evidence.text
            || evidence.source.is_empty()
            || evidence.source.trim() != evidence.source
            || evidence.provenance.is_empty()
            || evidence.provenance.trim() != evidence.provenance
        {
            return Err(
                "replacement-ledger overrides require exact nonempty text, source, and provenance"
                    .to_string(),
            );
        }
        authenticate_repository_override(repository_root, &evidence).await?;
        if !overrides.insert(evidence) {
            return Err("the replacement ledger contains a duplicate override".to_string());
        }
    }

    Ok((exceptions, overrides))
}

async fn authenticate_repository_override(
    repository_root: &Path,
    evidence: &CompletionOverrideEvidence,
) -> Result<(), String> {
    let snapshot = capture_repository_relaxation(repository_root).await;
    match verified_repository_relaxation(repository_root, &snapshot).await {
        CompletionProofRelaxationResolution::Clear {
            decision: CompletionProofRelaxationDecision::AllowWithoutCurrentProof,
            evidence: authenticated,
        } if authenticated.contains(evidence) => Ok(()),
        CompletionProofRelaxationResolution::Clear { .. } => Err(
            "a completion override was not an explicit repository instruction allowing completion without current proof"
                .to_string(),
        ),
        CompletionProofRelaxationResolution::Ambiguous { .. } => Err(
            "repository completion-proof instructions were ambiguous and cannot authenticate an override"
                .to_string(),
        ),
        CompletionProofRelaxationResolution::Invalid { message } => Err(format!(
            "repository completion-proof override provenance was invalid: {message}"
        )),
        CompletionProofRelaxationResolution::None => Err(
            "a completion override was not authenticated as a clear committed repository instruction"
                .to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenTestInventoryV1 {
    schema_version: u32,
    #[serde(default)]
    baseline_commit: Option<String>,
    #[serde(default)]
    baseline_workspace_fingerprint: Option<String>,
    #[serde(default)]
    host_platform: Option<String>,
    inventory_hash: String,
    tests: Vec<FrozenTestInventoryRowV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenTestInventoryRowV1 {
    baseline_id: String,
    framework: String,
    native_id: String,
    source: String,
    #[serde(default)]
    ignored: bool,
    #[serde(default)]
    platforms: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionProofAttemptReportV1 {
    schema_version: u32,
    report_type: String,
    policy_id: String,
    policy_runner_bundle_sha256: String,
    exact_command: String,
    nonce: String,
    attempt_id: String,
    parent_pid: u32,
    observed_runner_parent_pid: u32,
    runner_process_identity: RunnerProcessIdentity,
    repository_root: String,
    host_identity: HostIdentity,
    inventory_hash: String,
    start_fingerprint: String,
    end_fingerprint: String,
    start_mutation_epoch: u64,
    end_mutation_epoch: u64,
    attempt_classification: AttemptClassification,
    validations: Vec<ValidationAttemptReport>,
    child_processes: Vec<ChildProcessReport>,
    #[serde(default)]
    exceptions: Vec<BaselineExceptionEvidence>,
    #[serde(default)]
    overrides: Vec<CompletionOverrideEvidence>,
    workspace: WorkspaceObservation,
    fatal_error: String,
    #[serde(default)]
    focused_validation_id: Option<String>,
    attempt_report_hash: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct BaselineExceptionEvidence {
    baseline_id: String,
    kind: String,
    source: String,
    text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct BaselineExceptionProvenance {
    kind: String,
    source: String,
    text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionOverrideEvidence {
    text: String,
    source: String,
    provenance: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BaselineExceptionRule {
    id_prefix: Option<String>,
    source_prefix: Option<String>,
    kind: String,
    source: String,
    text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostIdentity {
    hostname: String,
    system: String,
    release: String,
    machine: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttemptClassification {
    ConfirmedPass,
    ConfirmedValidationFailure,
    PreResultError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValidationEvidenceKind {
    StructuredTest,
    TypedNonTest,
    InventoryReconciliation,
    Infrastructure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationEvidenceContract {
    runner: String,
    runner_selector: Option<String>,
    evidence_kind: ValidationEvidenceKind,
    validation_type: Option<String>,
    input_contract_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidationAttemptReport {
    id: String,
    execution_id: String,
    runner: String,
    runner_selector: Option<String>,
    evidence_kind: ValidationEvidenceKind,
    validation_type: Option<String>,
    #[serde(default)]
    input_contract_digest: Option<String>,
    classification: ValidationClassification,
    intended_ids: Vec<String>,
    selected_ids: Vec<String>,
    executed_ids: Vec<String>,
    intended_count: usize,
    selected_count: usize,
    executed_count: usize,
    outcomes: Vec<serde_json::Value>,
    exit_code: Option<i32>,
    diagnostic: String,
    confirmed_failure_ids: Vec<String>,
    report_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValidationClassification {
    ConfirmedPass,
    ConfirmedValidationFailure,
    PreResultError,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChildProcessReport {
    validation_id: String,
    execution_id: String,
    pid: u32,
    executable: String,
    launch_target_identity: LaunchTargetIdentity,
    args_hash: String,
    started_at: u64,
    ended_at: u64,
    exit_code: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchTargetIdentity {
    requested: String,
    resolved_path: String,
    sha256_before: String,
    sha256_after: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerProcessIdentity {
    pid: u32,
    parent_pid: u32,
    started_at: u64,
    ended_at: u64,
    args_hash: String,
    executable_identity: LaunchTargetIdentity,
    entrypoint_identity: LaunchTargetIdentity,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceObservation {
    observed_start_fingerprint: String,
    observed_end_fingerprint: String,
}

fn validate_report_hashes(report: &serde_json::Value) -> Result<(), String> {
    validate_embedded_hash(report, "attempt_report_hash", "the private attempt report")?;
    let validations = report
        .get("validations")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the private report did not contain a validations array".to_string())?;
    for validation in validations {
        let validation_id = validation
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unknown>");
        validate_embedded_hash(
            validation,
            "report_hash",
            &format!("validation {validation_id}"),
        )?;
    }
    Ok(())
}

fn validate_embedded_hash(
    value: &serde_json::Value,
    field: &str,
    label: &str,
) -> Result<(), String> {
    let expected = value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_sha256(value))
        .ok_or_else(|| format!("{label} omitted a valid {field}"))?;
    let mut canonical = value.clone();
    let object = canonical
        .as_object_mut()
        .ok_or_else(|| format!("{label} was not a JSON object"))?;
    object.remove(field);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| format!("{label} could not be hashed: {error}"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(format!("{label} hash did not match its contents"));
    }
    Ok(())
}

fn completion_artifact_hash(artifact: &CompletionProofArtifactV1) -> Result<String, String> {
    let mut value = serde_json::to_value(artifact)
        .map_err(|error| format!("the completion artifact could not be encoded: {error}"))?;
    value
        .as_object_mut()
        .ok_or_else(|| "the completion artifact was not an object".to_string())?
        .remove("artifact_hash");
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| format!("the completion artifact could not be hashed: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn refresh_completion_artifact_hash(artifact: &mut CompletionProofArtifactV1) {
    artifact.artifact_hash.clear();
    artifact.artifact_hash = completion_artifact_hash(artifact).unwrap_or_default();
}

fn completion_artifact_is_current(
    artifact: &CompletionProofArtifactV1,
    config: &RepositoryCompletionProofConfig,
    repository_root: &Path,
    workspace_fingerprint: &str,
    inventory_hash: &str,
    mutation_epoch: u64,
    policy_runner_bundle_sha256: &str,
) -> bool {
    let validation_ids = artifact
        .validations
        .iter()
        .map(|validation| validation.id.clone())
        .collect::<BTreeSet<_>>();
    artifact.schema_version == 1
        && artifact.artifact_type == "CompletionProofArtifactV1"
        && artifact.policy_id == config.policy_id
        && artifact.policy_runner_bundle_sha256 == policy_runner_bundle_sha256
        && is_sha256(&artifact.policy_runner_bundle_sha256)
        && artifact.exact_command == config.canonical_command
        && same_completion_proof_path(
            &canonical_repository_root(Path::new(&artifact.repository_root)),
            repository_root,
        )
        && artifact.workspace_fingerprint == workspace_fingerprint
        && artifact.end_fingerprint == artifact.start_fingerprint
        && artifact.start_mutation_epoch == artifact.end_mutation_epoch
        && artifact.end_mutation_epoch == artifact.mutation_epoch
        && artifact.mutation_epoch == mutation_epoch
        && artifact.inventory_hash == inventory_hash
        && artifact.invocation_nonce.len() >= 32
        && artifact.parent_pid > 0
        && artifact.observed_runner_parent_pid > 0
        && Uuid::parse_str(&artifact.attempt_id).is_ok()
        && is_sha256(&artifact.report_hash)
        && is_sha256(&artifact.attempt_report_hash)
        && is_sha256(&artifact.artifact_hash)
        && completion_artifact_hash(artifact).as_deref() == Ok(artifact.artifact_hash.as_str())
        && validation_ids == config.validation_ids
        && artifact.validations.len() == config.validation_ids.len()
        && !artifact.child_processes.is_empty()
}

async fn validate_attempt_start_envelope(
    pending: &PendingAttempt,
    report: &CompletionProofAttemptReportV1,
    expected_report_type: &str,
    runner_attestation: &RunnerProcessAttestation,
) -> Result<(), String> {
    if report.schema_version != ATTEMPT_SCHEMA_VERSION
        || report.report_type != expected_report_type
        || (expected_report_type == ATTEMPT_REPORT_TYPE && report.focused_validation_id.is_some())
    {
        return Err("the private report schema or report type was not trusted".to_string());
    }
    if report.policy_id != pending.expected_policy_id
        || report.policy_runner_bundle_sha256 != pending.policy_runner_bundle_sha256
        || !is_sha256(&report.policy_runner_bundle_sha256)
        || report.exact_command != pending.exact_command
        || report.nonce != pending.nonce
        || report.attempt_id != pending.attempt_id
        || report.parent_pid != pending.parent_pid
        || report.observed_runner_parent_pid == 0
        || !same_completion_proof_path(
            &canonical_repository_root(Path::new(&report.repository_root)),
            &pending.repository_root,
        )
        || report.start_fingerprint != pending.start_fingerprint
        || report.workspace.observed_start_fingerprint != pending.start_fingerprint
        || report.start_mutation_epoch != pending.start_mutation_epoch
    {
        return Err(
            "the private report was copied, replayed, or did not match this exact invocation"
                .to_string(),
        );
    }
    validate_runner_process_identity(pending, report, runner_attestation).await?;
    if report.inventory_hash != pending.expected_inventory_hash
        || !is_sha256(&report.inventory_hash)
        || !is_sha256(&report.attempt_report_hash)
    {
        return Err("the private report did not cover the current frozen inventory".to_string());
    }
    if report.host_identity.hostname.trim().is_empty()
        || report.host_identity.system.trim().is_empty()
        || report.host_identity.release.trim().is_empty()
        || report.host_identity.machine.trim().is_empty()
    {
        return Err("the private report omitted its host identity".to_string());
    }
    let report_exceptions = report.exceptions.iter().cloned().collect::<BTreeSet<_>>();
    if report_exceptions.len() != report.exceptions.len()
        || report_exceptions != pending.expected_exceptions
    {
        return Err(
            "the report exceptions did not exactly match the authenticated frozen-ledger provenance"
                .to_string(),
        );
    }
    let report_overrides = report.overrides.iter().cloned().collect::<BTreeSet<_>>();
    if report_overrides.len() != report.overrides.len()
        || report_overrides != pending.expected_overrides
    {
        return Err(
            "the report overrides did not exactly match authenticated completion instructions"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_attempt_completion_envelope(
    pending: &PendingAttempt,
    report: &CompletionProofAttemptReportV1,
) -> Result<(), String> {
    if report.end_mutation_epoch != pending.start_mutation_epoch
        || report.end_fingerprint != pending.start_fingerprint
        || report.workspace.observed_end_fingerprint != report.end_fingerprint
    {
        return Err("the workspace or mutation epoch changed during certification".to_string());
    }
    Ok(())
}

async fn validate_confirmed_pass(
    pending: &PendingAttempt,
    report: &CompletionProofAttemptReportV1,
) -> Result<(), String> {
    let actual_ids = report
        .validations
        .iter()
        .map(|validation| validation.id.clone())
        .collect::<BTreeSet<_>>();
    if actual_ids != pending.expected_validation_ids
        || report.validations.len() != pending.expected_validation_ids.len()
        || !report.fatal_error.is_empty()
    {
        return Err(
            "the canonical attempt did not report exactly every configured validation".to_string(),
        );
    }
    let mut execution_ids = BTreeSet::new();
    for validation in &report.validations {
        let Some(contract) = pending.validation_evidence_contracts.get(&validation.id) else {
            return Err(format!(
                "validation {} had no trusted evidence contract",
                validation.id
            ));
        };
        if validation.classification != ValidationClassification::ConfirmedPass
            || validation.exit_code != Some(0)
            || !validation_evidence_contract_matches(validation, contract)
            || !confirmed_pass_evidence_shape_matches(validation)
            || validation.diagnostic.len() > 8_000
            || !is_sha256(&validation.report_hash)
            || validation.execution_id.trim().is_empty()
            || !execution_ids.insert(validation.execution_id.clone())
        {
            return Err(format!(
                "validation {} did not prove a fresh intended nonzero selection and confirmed pass",
                validation.id
            ));
        }
        let matching_children = report
            .child_processes
            .iter()
            .filter(|child| {
                child.validation_id == validation.id
                    && child.execution_id == validation.execution_id
            })
            .collect::<Vec<_>>();
        if matching_children.len() != 1 {
            return Err(format!(
                "validation {} does not have exactly one matching child runner identity",
                validation.id
            ));
        }
        validate_child_identity(pending, matching_children[0], Some(0)).await?;
    }
    if report.child_processes.len() != report.validations.len() {
        return Err(
            "the confirmed attempt reported extra, duplicate, or missing child runner identities"
                .to_string(),
        );
    }
    Ok(())
}

async fn validate_confirmed_failure(
    pending: &PendingAttempt,
    report: &CompletionProofAttemptReportV1,
    validation: &ValidationAttemptReport,
) -> Result<(), String> {
    let Some(contract) = pending.validation_evidence_contracts.get(&validation.id) else {
        return Err(format!(
            "validation {} had no trusted evidence contract",
            validation.id
        ));
    };
    if !pending.expected_validation_ids.contains(&validation.id)
        || validation.exit_code.is_none_or(|exit_code| exit_code == 0)
        || !validation_evidence_contract_matches(validation, contract)
        || !confirmed_failure_evidence_shape_matches(validation)
        || validation.diagnostic.len() > 8_000
        || !is_sha256(&validation.report_hash)
        || validation.execution_id.trim().is_empty()
        || report
            .validations
            .iter()
            .filter(|other| {
                other.execution_id == validation.execution_id && other.id != validation.id
            })
            .count()
            != 0
    {
        return Err(format!(
            "validation {} did not prove that its intended nonzero selection actually executed and returned a validation failure",
            validation.id
        ));
    }
    let matching_children = report
        .child_processes
        .iter()
        .filter(|child| {
            child.validation_id == validation.id && child.execution_id == validation.execution_id
        })
        .collect::<Vec<_>>();
    if matching_children.len() != 1 {
        return Err(format!(
            "validation {} does not have exactly one matching failed child runner identity",
            validation.id
        ));
    }
    validate_child_identity(pending, matching_children[0], validation.exit_code).await?;
    Ok(())
}

fn confirmed_pass_outcomes_match(validation: &ValidationAttemptReport) -> bool {
    if !validation.confirmed_failure_ids.is_empty()
        || validation.outcomes.len() != validation.executed_count
    {
        return false;
    }
    let mut observed = BTreeSet::new();
    for outcome in &validation.outcomes {
        let Some(id) = outcome.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        if outcome.get("outcome").and_then(serde_json::Value::as_str) != Some("passed")
            || !observed.insert(id.to_string())
        {
            return false;
        }
    }
    observed == validation.executed_ids.iter().cloned().collect()
}

fn validation_evidence_contract_matches(
    validation: &ValidationAttemptReport,
    contract: &ValidationEvidenceContract,
) -> bool {
    validation.runner == contract.runner
        && validation.runner_selector == contract.runner_selector
        && validation.evidence_kind == contract.evidence_kind
        && validation.validation_type == contract.validation_type
        && validation.input_contract_digest == contract.input_contract_digest
        && validation.evidence_kind != ValidationEvidenceKind::Infrastructure
}

fn confirmed_pass_evidence_shape_matches(validation: &ValidationAttemptReport) -> bool {
    match validation.evidence_kind {
        ValidationEvidenceKind::StructuredTest
        | ValidationEvidenceKind::InventoryReconciliation => {
            validation.intended_count > 0
                && validation.selected_count > 0
                && validation.executed_count > 0
                && validation.intended_count == validation.intended_ids.len()
                && validation.selected_count == validation.selected_ids.len()
                && validation.executed_count == validation.executed_ids.len()
                && validation.intended_ids == validation.selected_ids
                && validation.selected_ids == validation.executed_ids
                && confirmed_pass_outcomes_match(validation)
        }
        ValidationEvidenceKind::TypedNonTest => {
            validation.intended_count == 1
                && validation.selected_count == 1
                && validation.executed_count == 1
                && validation.intended_ids == vec![validation.id.clone()]
                && validation.selected_ids == validation.intended_ids
                && validation.executed_ids == validation.intended_ids
                && confirmed_pass_outcomes_match(validation)
        }
        ValidationEvidenceKind::Infrastructure => false,
    }
}

fn confirmed_failure_outcomes_match(validation: &ValidationAttemptReport) -> bool {
    if validation.outcomes.len() != validation.executed_count {
        return false;
    }
    let mut observed = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for outcome in &validation.outcomes {
        let Some(id) = outcome.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(classification) = outcome.get("outcome").and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        if !matches!(classification, "passed" | "failed") || !observed.insert(id.to_string()) {
            return false;
        }
        if classification == "failed" {
            failed.insert(id.to_string());
        }
    }
    let reported_failures = validation
        .confirmed_failure_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    !failed.is_empty()
        && reported_failures.len() == validation.confirmed_failure_ids.len()
        && reported_failures == failed
        && observed == validation.executed_ids.iter().cloned().collect()
}

fn confirmed_failure_evidence_shape_matches(validation: &ValidationAttemptReport) -> bool {
    match validation.evidence_kind {
        ValidationEvidenceKind::StructuredTest
        | ValidationEvidenceKind::InventoryReconciliation => {
            validation.intended_count > 0
                && validation.selected_count > 0
                && validation.executed_count > 0
                && validation.intended_count == validation.intended_ids.len()
                && validation.selected_count == validation.selected_ids.len()
                && validation.executed_count == validation.executed_ids.len()
                && validation.intended_ids == validation.selected_ids
                && validation
                    .executed_ids
                    .iter()
                    .all(|id| validation.selected_ids.contains(id))
                && confirmed_failure_outcomes_match(validation)
        }
        ValidationEvidenceKind::TypedNonTest => {
            validation.intended_count == 1
                && validation.selected_count == 1
                && validation.executed_count == 1
                && validation.intended_ids == vec![validation.id.clone()]
                && validation.selected_ids == validation.intended_ids
                && validation.executed_ids == validation.intended_ids
                && confirmed_failure_outcomes_match(validation)
        }
        ValidationEvidenceKind::Infrastructure => false,
    }
}

async fn validate_child_identity(
    pending: &PendingAttempt,
    child: &ChildProcessReport,
    expected_exit_code: Option<i32>,
) -> Result<(), String> {
    let earliest_start = pending.started_at_unix_ms.saturating_mul(1_000_000);
    if child.pid == 0
        || child.executable.trim().is_empty()
        || !is_sha256(&child.args_hash)
        || child.started_at < earliest_start
        || child.ended_at < child.started_at
        || child.exit_code != expected_exit_code
    {
        return Err(format!(
            "validation {} did not record a fresh child process with the expected result",
            child.validation_id
        ));
    }
    validate_launch_target_identity(
        &child.launch_target_identity,
        &format!("validation {} child launch target", child.validation_id),
    )
    .await?;
    if canonical_existing_path(Path::new(&child.executable)).await?
        != canonical_existing_path(Path::new(&child.launch_target_identity.resolved_path)).await?
    {
        return Err(format!(
            "validation {} child executable did not match its resolved launch identity",
            child.validation_id
        ));
    }
    Ok(())
}

async fn validate_runner_process_identity(
    pending: &PendingAttempt,
    report: &CompletionProofAttemptReportV1,
    attestation: &RunnerProcessAttestation,
) -> Result<(), String> {
    let identity = &report.runner_process_identity;
    let earliest_start = pending.started_at_unix_ms.saturating_mul(1_000_000);
    if identity.pid == 0
        || identity.parent_pid == 0
        || identity.parent_pid != report.observed_runner_parent_pid
        || identity.started_at < earliest_start
        || identity.ended_at < identity.started_at
        || !is_sha256(&identity.args_hash)
    {
        return Err(
            "the canonical runner omitted a fresh internally consistent process identity"
                .to_string(),
        );
    }
    validate_launch_target_identity(
        &identity.executable_identity,
        "the canonical runner executable",
    )
    .await?;
    validate_launch_target_identity(
        &identity.entrypoint_identity,
        "the canonical runner entrypoint",
    )
    .await?;
    let resolved_executable =
        canonical_existing_path(Path::new(&identity.executable_identity.resolved_path)).await?;
    let resolved_entrypoint =
        canonical_existing_path(Path::new(&identity.entrypoint_identity.resolved_path)).await?;
    let process_matches = identity.pid == attestation.process_id;
    let executable_matches = existing_paths_refer_to_same_file(
        &resolved_executable,
        &attestation.executable_path,
        "the canonical runner executable",
    )?;
    let entrypoint_matches = existing_paths_refer_to_same_file(
        &resolved_entrypoint,
        &attestation.entrypoint_path,
        "the canonical runner entrypoint",
    )?;
    if !process_matches || !executable_matches || !entrypoint_matches {
        return Err(
            "the canonical runner report did not match the operating-system-authenticated process and entrypoint"
                .to_string(),
        );
    }
    let repository_root = tokio::fs::canonicalize(&pending.repository_root)
        .await
        .map_err(|error| {
            format!(
                "could not resolve completion-proof repository root {}: {error}",
                pending.repository_root.display()
            )
        })?;
    let relative_entrypoint = resolved_entrypoint
        .strip_prefix(&repository_root)
        .map_err(|_| {
            format!(
                "the canonical runner entrypoint {} was outside repository {}",
                resolved_entrypoint.display(),
                repository_root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let trusted_entrypoint = TrustedRunnerEntrypoint {
        relative_path: relative_entrypoint,
        sha256: identity.entrypoint_identity.sha256_after.clone(),
    };
    if !pending
        .trusted_runner_entrypoints
        .contains(&trusted_entrypoint)
    {
        return Err(
            "the canonical runner entrypoint was not one of the immutable entrypoints authorized by the active completion-proof policy"
                .to_string(),
        );
    }
    Ok(())
}

async fn canonical_existing_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "launch identity path {} was not absolute",
            path.display()
        ));
    }
    tokio::fs::canonicalize(path).await.map_err(|error| {
        format!(
            "could not resolve launch identity path {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(windows))]
fn existing_paths_refer_to_same_file(
    left: &Path,
    right: &Path,
    _label: &str,
) -> Result<bool, String> {
    Ok(left == right)
}

#[cfg(windows)]
fn existing_paths_refer_to_same_file(
    left: &Path,
    right: &Path,
    label: &str,
) -> Result<bool, String> {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    fn file_identity(path: &Path, label: &str) -> Result<(u32, u32, u32), String> {
        let file = File::open(path).map_err(|error| {
            format!(
                "could not open {label} {} for identity checking: {error}",
                path.display()
            )
        })?;
        let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        let success = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
        if success == 0 {
            return Err(format!(
                "could not obtain the Windows file identity for {label} {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        Ok((
            information.dwVolumeSerialNumber,
            information.nFileIndexHigh,
            information.nFileIndexLow,
        ))
    }

    Ok(file_identity(left, label)? == file_identity(right, label)?)
}

async fn validate_launch_target_identity(
    identity: &LaunchTargetIdentity,
    label: &str,
) -> Result<(), String> {
    if identity.requested.trim().is_empty()
        || identity.resolved_path.trim().is_empty()
        || !is_sha256(&identity.sha256_before)
        || identity.sha256_before != identity.sha256_after
    {
        return Err(format!(
            "{label} omitted a stable requested path, resolved path, or before/after hash"
        ));
    }
    let resolved_path = Path::new(&identity.resolved_path);
    let _ = canonical_existing_path(resolved_path).await?;
    let metadata = tokio::fs::symlink_metadata(resolved_path)
        .await
        .map_err(|error| {
            format!(
                "could not inspect {label} {}: {error}",
                resolved_path.display()
            )
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{label} {} was not a regular non-symlink file",
            resolved_path.display()
        ));
    }
    let bytes = tokio::fs::read(resolved_path).await.map_err(|error| {
        format!(
            "could not read {label} {}: {error}",
            resolved_path.display()
        )
    })?;
    let current_hash = format!("{:x}", Sha256::digest(bytes));
    if current_hash != identity.sha256_after {
        return Err(format!(
            "{label} {} changed after the trusted runner observed it",
            resolved_path.display()
        ));
    }
    Ok(())
}

async fn poisoned_validation_is_eligible_at_snapshot(
    repository_root: &Path,
    poisoned_validation: &PoisonedValidation,
    current_mutation_epoch: u64,
    expected_workspace_fingerprint: &str,
) -> Result<bool, String> {
    if current_mutation_epoch <= poisoned_validation.failed_at_mutation_epoch
        || poisoned_validation.relevant_input_contract.is_none()
    {
        return Ok(false);
    }
    let Some(failure_input_snapshot) = poisoned_validation.failure_input_snapshot.as_ref() else {
        // State written before input snapshots were introduced cannot become
        // eligible by guessing what the failed validation consumed.
        return Ok(false);
    };
    let input_contract = poisoned_validation
        .relevant_input_contract
        .as_ref()
        .expect("checked above");
    let (observation, current_snapshot) =
        stable_validation_input_snapshot(repository_root, input_contract).await;
    let Some(observation) = observation else {
        return Err(format!(
            "validation {} remained poisoned because the runtime could not observe the current repository inputs",
            poisoned_validation.validation_id
        ));
    };
    if observation.fingerprint != expected_workspace_fingerprint {
        return Err(format!(
            "validation {} remained poisoned because the repository changed while its current inputs were checked",
            poisoned_validation.validation_id
        ));
    }
    let Some(current_snapshot) = current_snapshot else {
        return Err(format!(
            "validation {} remained poisoned because the runtime could not establish a stable current input snapshot",
            poisoned_validation.validation_id
        ));
    };
    Ok(&current_snapshot != failure_input_snapshot)
}

fn normalized_validation_pattern(pattern: &str) -> String {
    let mut normalized = pattern.replace('\\', "/");
    if cfg!(windows) {
        normalized.make_ascii_lowercase();
    }
    normalized
}

async fn stable_validation_input_snapshots(
    repository_root: &Path,
    validation_patterns: &BTreeMap<String, ValidationInputContract>,
) -> (
    Option<WorkspaceObservationSnapshot>,
    Option<BTreeMap<String, ValidationInputSnapshotV1>>,
) {
    if validation_patterns.is_empty() {
        return (workspace_observation(repository_root).await, None);
    }
    let mut latest_observation = None;
    for _ in 0..VALIDATION_INPUT_SNAPSHOT_ATTEMPTS {
        let Some(before) = workspace_observation(repository_root).await else {
            return (None, None);
        };
        let mut first = BTreeMap::new();
        let mut second = BTreeMap::new();
        let mut complete = true;
        for (validation_id, patterns) in validation_patterns {
            match validation_input_snapshot(repository_root, patterns).await {
                Some(snapshot) => {
                    first.insert(validation_id.clone(), snapshot);
                }
                None => complete = false,
            }
        }
        for (validation_id, patterns) in validation_patterns {
            match validation_input_snapshot(repository_root, patterns).await {
                Some(snapshot) => {
                    second.insert(validation_id.clone(), snapshot);
                }
                None => complete = false,
            }
        }
        let Some(after) = workspace_observation(repository_root).await else {
            return (None, None);
        };
        let stable = complete
            && before.fingerprint == after.fingerprint
            && before.head_identity == after.head_identity
            && before.path_fingerprints == after.path_fingerprints
            && first.len() == validation_patterns.len()
            && first == second;
        latest_observation = Some(after);
        if stable {
            return (latest_observation, Some(first));
        }
    }
    (latest_observation, None)
}

async fn stable_validation_input_snapshot(
    repository_root: &Path,
    contract: &ValidationInputContract,
) -> (
    Option<WorkspaceObservationSnapshot>,
    Option<ValidationInputSnapshotV1>,
) {
    let mut latest_observation = None;
    for _ in 0..VALIDATION_INPUT_SNAPSHOT_ATTEMPTS {
        let Some(before) = workspace_observation(repository_root).await else {
            return (None, None);
        };
        let first = validation_input_snapshot(repository_root, contract).await;
        let second = validation_input_snapshot(repository_root, contract).await;
        let Some(after) = workspace_observation(repository_root).await else {
            return (None, None);
        };
        let stable = before.fingerprint == after.fingerprint
            && before.head_identity == after.head_identity
            && before.path_fingerprints == after.path_fingerprints
            && first.is_some()
            && first == second;
        latest_observation = Some(after);
        if stable {
            return (latest_observation, first);
        }
    }
    (latest_observation, None)
}

async fn validation_input_snapshot(
    repository_root: &Path,
    contract: &ValidationInputContract,
) -> Option<ValidationInputSnapshotV1> {
    if contract.schema_version != 1
        || (contract.content_paths.is_empty() && contract.path_set_paths.is_empty())
    {
        return None;
    }
    let mut normalized_content_patterns = contract
        .content_paths
        .iter()
        .map(|pattern| normalized_validation_pattern(pattern))
        .collect::<BTreeSet<_>>();
    let normalized_path_set_patterns = contract
        .path_set_paths
        .iter()
        .map(|pattern| normalized_validation_pattern(pattern))
        .collect::<BTreeSet<_>>();
    for manifest_path in &contract.evidence_path_manifests {
        let manifest_path = normalized_validation_pattern(manifest_path);
        let manifest = repository_root.join(Path::new(&manifest_path));
        let manifest_contents = tokio::fs::read_to_string(&manifest).await.ok()?;
        let manifest_value = toml::from_str::<toml::Value>(&manifest_contents).ok()?;
        collect_symbol_evidence_paths(&manifest_value, &mut normalized_content_patterns)?;
    }
    let compiled_content_patterns = normalized_content_patterns
        .iter()
        .map(|pattern| glob::Pattern::new(pattern).ok())
        .collect::<Option<Vec<_>>>()?;
    let compiled_path_set_patterns = normalized_path_set_patterns
        .iter()
        .map(|pattern| glob::Pattern::new(pattern).ok())
        .collect::<Option<Vec<_>>>()?;
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "-c",
            disabled_hooks_argument(),
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository_root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn()).ok()?;
    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let mut candidates = strict_nul_separated_paths(&output.stdout)?;
    for pattern in normalized_content_patterns
        .iter()
        .chain(normalized_path_set_patterns.iter())
    {
        if !pattern
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'['))
        {
            let literal = Path::new(pattern);
            if !safe_repository_relative_path(literal) {
                return None;
            }
            candidates.insert(normalize_git_relative_path(literal));
        }
    }
    let matching_content_paths = candidates
        .iter()
        .filter(|path| {
            compiled_content_patterns
                .iter()
                .any(|pattern| pattern.matches(path))
        })
        .cloned()
        .collect::<Vec<_>>();
    let matching_path_set_paths = candidates
        .into_iter()
        .filter(|path| {
            compiled_path_set_patterns
                .iter()
                .any(|pattern| pattern.matches(path))
        })
        .collect::<Vec<_>>();
    let repository_root = repository_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut snapshot = Sha256::new();
        snapshot.update(b"KD4_VALIDATION_INPUT_SNAPSHOT_V2\0");
        for pattern in normalized_content_patterns {
            update_snapshot_frame(&mut snapshot, pattern.as_bytes());
        }
        update_snapshot_frame(&mut snapshot, b"PATH_SET_PATTERNS");
        for pattern in normalized_path_set_patterns {
            update_snapshot_frame(&mut snapshot, pattern.as_bytes());
        }
        update_snapshot_frame(&mut snapshot, b"CONTENT_PATHS");
        for path in matching_content_paths {
            let absolute = repository_root.join(Path::new(&path));
            let (kind, length, content_sha256) = validation_path_identity(&absolute)?;
            update_snapshot_frame(&mut snapshot, path.as_bytes());
            update_snapshot_frame(&mut snapshot, kind);
            update_snapshot_frame(&mut snapshot, &length.to_be_bytes());
            update_snapshot_frame(&mut snapshot, content_sha256.as_bytes());
        }
        update_snapshot_frame(&mut snapshot, b"PATH_SET");
        for path in matching_path_set_paths {
            let absolute = repository_root.join(Path::new(&path));
            let (kind, _, _) = validation_path_identity(&absolute)?;
            update_snapshot_frame(&mut snapshot, path.as_bytes());
            update_snapshot_frame(&mut snapshot, kind);
        }
        Some(ValidationInputSnapshotV1 {
            schema_version: VALIDATION_INPUT_SNAPSHOT_SCHEMA_VERSION,
            algorithm: VALIDATION_INPUT_SNAPSHOT_ALGORITHM.to_string(),
            sha256: format!("{:x}", snapshot.finalize()),
        })
    })
    .await
    .ok()
    .flatten()
}

fn collect_symbol_evidence_paths(value: &toml::Value, paths: &mut BTreeSet<String>) -> Option<()> {
    match value {
        toml::Value::Array(values) => {
            for value in values {
                collect_symbol_evidence_paths(value, paths)?;
            }
        }
        toml::Value::Table(table) => {
            if table
                .get("symbol")
                .and_then(toml::Value::as_str)
                .is_some_and(|symbol| !symbol.is_empty())
            {
                let path = table.get("path").and_then(toml::Value::as_str)?;
                let normalized = normalized_validation_pattern(path);
                if !safe_repository_relative_path(Path::new(&normalized))
                    || normalized
                        .bytes()
                        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
                {
                    return None;
                }
                paths.insert(normalized);
            }
            for value in table.values() {
                collect_symbol_evidence_paths(value, paths)?;
            }
        }
        _ => {}
    }
    Some(())
}

fn update_snapshot_frame(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn validation_path_identity(path: &Path) -> Option<(&'static [u8], u64, String)> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Some((b"missing", 0, format!("{:x}", Sha256::digest([]))));
        }
        Err(_) => return None,
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        let bytes = target.as_os_str().as_encoded_bytes();
        return Some((
            b"symlink",
            bytes.len().try_into().ok()?,
            format!("{:x}", Sha256::digest(bytes)),
        ));
    }
    if metadata.is_file() {
        let mut file = std::fs::File::open(path).ok()?;
        let mut hasher = Sha256::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            length = length.checked_add(read.try_into().ok()?)?;
            hasher.update(&buffer[..read]);
        }
        return Some((b"file", length, format!("{:x}", hasher.finalize())));
    }
    if metadata.is_dir() {
        return Some((b"directory", 0, format!("{:x}", Sha256::digest([]))));
    }
    None
}

fn safe_repository_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
        && normalize_git_relative_path(path) != ".git"
        && !normalize_git_relative_path(path).starts_with(".git/")
}

fn strict_nul_separated_paths(bytes: &[u8]) -> Option<BTreeSet<String>> {
    if bytes.is_empty() {
        return Some(BTreeSet::new());
    }
    if bytes.last() != Some(&0) {
        return None;
    }

    let mut paths = BTreeSet::new();
    for raw in bytes[..bytes.len().saturating_sub(1)].split(|byte| *byte == 0) {
        let path = strict_git_output_path(raw)?;
        if !paths.insert(path) {
            return None;
        }
    }
    Some(paths)
}

fn strict_git_output_path(raw_path: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(raw_path).ok()?;
    let path = Path::new(path);
    if !safe_repository_relative_path(path)
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let normalized = normalize_git_relative_path(path);
    if normalized == "."
        || normalized.eq_ignore_ascii_case(".git")
        || normalized
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".git/"))
    {
        return None;
    }
    Some(normalized)
}

#[derive(Clone, Debug)]
struct WorkspaceObservationSnapshot {
    fingerprint: String,
    head_identity: Option<String>,
    path_fingerprints: BTreeMap<String, String>,
}

async fn workspace_observation(repository_root: &Path) -> Option<WorkspaceObservationSnapshot> {
    let identity = capture_workspace_evidence_identity(repository_root).await?;
    let worktree_identity = identity.worktree_identity?;
    let fingerprint =
        completion_workspace_fingerprint(identity.head_identity.as_deref(), &worktree_identity);
    let path_fingerprints = workspace_path_fingerprints(repository_root).await?;
    Some(WorkspaceObservationSnapshot {
        fingerprint,
        head_identity: identity.head_identity,
        path_fingerprints,
    })
}

async fn workspace_path_fingerprints(repository_root: &Path) -> Option<BTreeMap<String, String>> {
    let paths = changed_workspace_paths(repository_root).await?;
    let repository_root = repository_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut fingerprints = BTreeMap::new();
        for path in paths {
            let normalized = path.replace('\\', "/");
            let absolute = repository_root.join(&path);
            let mut hasher = Sha256::new();
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    hasher.update(b"symlink\0");
                    let target = std::fs::read_link(&absolute).ok()?;
                    hasher.update(target.to_string_lossy().as_bytes());
                }
                Ok(metadata) if metadata.is_file() => {
                    hasher.update(b"file\0");
                    hasher.update(std::fs::read(&absolute).ok()?);
                }
                Ok(metadata) if metadata.is_dir() => {
                    hasher.update(b"directory\0");
                }
                Ok(_) => {
                    hasher.update(b"other\0");
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    hasher.update(b"missing\0");
                }
                Err(_) => return None,
            }
            fingerprints.insert(normalized, format!("{:x}", hasher.finalize()));
        }
        Some(fingerprints)
    })
    .await
    .ok()
    .flatten()
}

fn changed_observation_paths(
    previous: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    previous
        .keys()
        .chain(current.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| previous.get(*path) != current.get(*path))
        .cloned()
        .collect()
}

fn union_mutation_paths(
    explicit: Option<&[String]>,
    observed: Option<&[String]>,
) -> Option<Vec<String>> {
    let paths = explicit
        .into_iter()
        .flatten()
        .chain(observed.into_iter().flatten())
        .cloned()
        .collect::<BTreeSet<_>>();
    (!paths.is_empty()).then(|| paths.into_iter().collect())
}

async fn changed_paths_between_heads(
    repository_root: &Path,
    previous_head: Option<&str>,
    current_head: Option<&str>,
) -> Option<Vec<String>> {
    if previous_head == current_head {
        return Some(Vec::new());
    }
    if previous_head.is_some_and(|identity| !valid_git_object_identity(identity))
        || current_head.is_some_and(|identity| !valid_git_object_identity(identity))
    {
        return None;
    }

    let mut command = tokio::process::Command::new("git");
    command
        .arg("-c")
        .arg(disabled_hooks_argument())
        .arg("-c")
        .arg("core.fsmonitor=false");
    match (previous_head, current_head) {
        (Some(previous), Some(current)) => {
            command.args([
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                previous,
                current,
                "--",
                ".",
            ]);
        }
        (None, Some(current)) => {
            command.args(["ls-tree", "-r", "--name-only", "-z", current]);
        }
        (Some(previous), None) => {
            command.args(["ls-tree", "-r", "--name-only", "-z", previous]);
        }
        (None, None) => return Some(Vec::new()),
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository_root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn()).ok()?;
    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        strict_nul_separated_paths(&output.stdout)?
            .into_iter()
            .collect(),
    )
}

fn valid_git_object_identity(identity: &str) -> bool {
    matches!(identity.len(), 40 | 64) && identity.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn reconcile_external_workspace_change(
    repository_root: &Path,
    state: &mut PersistentCompletionProofState,
    current_observation: Option<WorkspaceObservationSnapshot>,
) {
    if !same_completion_proof_path(Path::new(&state.repository_root), repository_root) {
        *state = PersistentCompletionProofState::new(repository_root);
    }
    let Some(current_observation) = current_observation else {
        // Losing observation cannot prove a new mutation epoch and must never
        // make poisoned validation eligible. It also cannot prove that a fresh
        // or previously clean workspace is unchanged, so every observation
        // failure blocks successful completion until a later exact canonical
        // attempt establishes current proof.
        state.requires_non_documentation_proof = true;
        state.requires_documentation_validation = false;
        state.registered_proof = None;
        return;
    };
    let had_observed_fingerprint = state.last_observed_fingerprint.is_some();
    let current_fingerprint = current_observation.fingerprint.clone();
    if state.last_observed_fingerprint.as_deref() == Some(current_fingerprint.as_str()) {
        state.last_observed_head_identity = current_observation.head_identity;
        state.last_observed_head_identity_initialized = true;
        state.last_observed_path_fingerprints = current_observation.path_fingerprints;
        return;
    }
    let dirty_paths = changed_observation_paths(
        &state.last_observed_path_fingerprints,
        &current_observation.path_fingerprints,
    );
    let committed_paths =
        if had_observed_fingerprint && state.last_observed_head_identity_initialized {
            changed_paths_between_heads(
                repository_root,
                state.last_observed_head_identity.as_deref(),
                current_observation.head_identity.as_deref(),
            )
            .await
        } else {
            // A state written before HEAD tracking was introduced has no trusted
            // previous commit identity. It may stale proof, but it must not clear
            // a poisoned validation by guessing which committed paths changed.
            None
        };
    let changed_paths = if had_observed_fingerprint {
        committed_paths.as_deref().map(|committed| {
            union_mutation_paths(Some(&dirty_paths), Some(committed)).unwrap_or_default()
        })
    } else {
        // A fresh private state must not bless an already-dirty checkout as its
        // clean baseline. Those paths are the observable mutation universe at
        // lineage start, including deletions and both endpoints of renames.
        Some(
            current_observation
                .path_fingerprints
                .keys()
                .cloned()
                .collect(),
        )
    };
    let documentation_only = changed_paths
        .as_ref()
        .is_some_and(|paths| !paths.is_empty() && paths.iter().all(|path| is_documentation(path)));
    if documentation_only {
        if !state.requires_non_documentation_proof {
            state.requires_documentation_validation = true;
            if let Some(proof) = state.registered_proof.as_mut() {
                proof.workspace_fingerprint = current_fingerprint.clone();
                refresh_completion_artifact_hash(proof);
            }
        }
    } else if had_observed_fingerprint
        || state.registered_proof.is_some()
        || changed_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
    {
        state.mutation_epoch = state.mutation_epoch.saturating_add(1);
        state.requires_non_documentation_proof = true;
        state.requires_documentation_validation = false;
        state.registered_proof = None;
    }
    state.last_observed_fingerprint = Some(current_fingerprint);
    state.last_observed_head_identity = current_observation.head_identity;
    state.last_observed_head_identity_initialized = true;
    state.last_observed_path_fingerprints = current_observation.path_fingerprints;
}

async fn canonical_attempt_observed_relevant_change(
    repository_root: &Path,
    observed_paths: &[PathBuf],
) -> Option<bool> {
    let paths = observed_relevant_paths(repository_root, observed_paths).await?;
    Some(!paths.is_empty())
}

async fn observed_relevant_paths(
    repository_root: &Path,
    observed_paths: &[PathBuf],
) -> Option<Vec<String>> {
    let mut candidates = BTreeSet::new();
    for path in observed_paths {
        let relative = if path.is_absolute() {
            path.strip_prefix(repository_root).ok()?
        } else {
            path.as_path()
        };
        let normalized = normalize_git_relative_path(relative);
        if normalized.is_empty()
            || normalized == "."
            || normalized == ".."
            || normalized.starts_with("../")
        {
            return None;
        }
        if normalized == ".git" || normalized.starts_with(".git/") {
            continue;
        }
        candidates.insert(normalized);
    }
    if candidates.is_empty() {
        return Some(Vec::new());
    }

    let mut tracked_command = tokio::process::Command::new("git");
    tracked_command
        .args([
            "-c",
            disabled_hooks_argument(),
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--cached",
            "-z",
            "--",
            ".",
        ])
        .current_dir(repository_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let tracked_child =
        codex_utils_pty::with_windows_child_creation(|_| tracked_command.spawn()).ok()?;
    let tracked_output = tracked_child.wait_with_output().await.ok()?;
    if !tracked_output.status.success() {
        return None;
    }
    let tracked = strict_nul_separated_paths(&tracked_output.stdout)?;
    let tracked_candidates = candidates
        .iter()
        .filter(|candidate| {
            tracked.iter().any(|tracked_path| {
                tracked_path == *candidate
                    || tracked_path
                        .strip_prefix(*candidate)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        })
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "-c",
            disabled_hooks_argument(),
            "-c",
            "core.fsmonitor=false",
            "check-ignore",
            "--no-index",
            "--stdin",
            "-z",
        ])
        .current_dir(repository_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = codex_utils_pty::with_windows_child_creation(|_| command.spawn()).ok()?;
    let mut input = Vec::new();
    for candidate in &candidates {
        input.extend_from_slice(candidate.as_bytes());
        input.push(0);
    }
    child.stdin.take()?.write_all(&input).await.ok()?;
    let output = child.wait_with_output().await.ok()?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return None;
    }
    let ignored = strict_nul_separated_paths(&output.stdout)?;
    Some(
        candidates
            .into_iter()
            .filter(|candidate| {
                tracked_candidates.contains(candidate) || !ignored.contains(candidate)
            })
            .collect(),
    )
}

fn normalize_git_relative_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

async fn changed_workspace_paths(repository_root: &Path) -> Option<Vec<String>> {
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "-c",
            disabled_hooks_argument(),
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository_root)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn()).ok()?;
    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    workspace_generation_paths(&output.stdout)
}

fn workspace_generation_paths(status: &[u8]) -> Option<Vec<String>> {
    if status.is_empty() {
        return Some(Vec::new());
    }
    if status.last() != Some(&0) {
        return None;
    }

    let records = status[..status.len().saturating_sub(1)]
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            return None;
        }
        match record.first().copied() {
            Some(b'1') => {
                paths.insert(strict_porcelain_path(porcelain_record_path(record, 8)?)?);
            }
            Some(b'2') => {
                paths.insert(strict_porcelain_path(porcelain_record_path(record, 9)?)?);
                index = index.checked_add(1)?;
                paths.insert(strict_porcelain_path(*records.get(index)?)?);
            }
            Some(b'u') => {
                paths.insert(strict_porcelain_path(porcelain_record_path(record, 10)?)?);
            }
            Some(b'?') if record.get(1) == Some(&b' ') => {
                paths.insert(strict_porcelain_path(record.get(2..)?)?);
            }
            _ => return None,
        }
        index = index.checked_add(1)?;
    }
    Some(paths.into_iter().collect())
}

fn porcelain_record_path(record: &[u8], field_index: usize) -> Option<&[u8]> {
    if record.get(1) != Some(&b' ') {
        return None;
    }
    record
        .splitn(field_index.saturating_add(1), |byte| *byte == b' ')
        .nth(field_index)
}

fn strict_porcelain_path(raw_path: &[u8]) -> Option<String> {
    strict_git_output_path(raw_path)
}

fn disabled_hooks_argument() -> &'static str {
    if cfg!(windows) {
        "core.hooksPath=NUL"
    } else {
        "core.hooksPath=/dev/null"
    }
}

fn completion_workspace_fingerprint(
    head_identity: Option<&str>,
    worktree_identity: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"KD4_COMPLETION_PROOF_WORKSPACE_V3\0");
    match head_identity {
        Some(head_identity) => {
            hasher.update(b"head\0");
            hasher.update(head_identity.as_bytes());
        }
        None => hasher.update(b"unborn-head\0"),
    }
    hasher.update(b"\0worktree\0");
    hasher.update(worktree_identity.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonical_repository_root(cwd: &Path) -> PathBuf {
    let root = codex_git_utils::get_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    dunce::canonicalize(&root).unwrap_or(root)
}

fn completion_proof_path_identity(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let mut identity = path.to_string_lossy().into_owned();
        identity.make_ascii_lowercase();
        PathBuf::from(identity)
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn same_completion_proof_path(left: &Path, right: &Path) -> bool {
    completion_proof_path_identity(left) == completion_proof_path_identity(right)
}

fn repository_state_key(repository_root: &Path) -> String {
    let mut identity = repository_root.to_string_lossy().into_owned();
    if cfg!(windows) {
        identity.make_ascii_lowercase();
    }
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

fn is_documentation(path: &str) -> bool {
    let normalized = path.trim_start_matches("./").replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("docs/")
        || lower.starts_with("doc/")
        || lower.starts_with("documentation/")
    {
        return has_documentation_prose_suffix(&lower);
    }

    let file_name = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let basename = match file_name.rsplit_once('.') {
        Some((stem, extension)) if is_documentation_prose_extension(extension) => stem,
        Some(_) => return false,
        None => file_name,
    };
    matches!(
        basename,
        "readme"
            | "changelog"
            | "changes"
            | "contributing"
            | "contributors"
            | "license"
            | "licence"
            | "notice"
            | "security"
            | "code_of_conduct"
            | "code-of-conduct"
    )
}

fn has_documentation_prose_suffix(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_stem, extension)| is_documentation_prose_extension(extension))
}

fn is_documentation_prose_extension(extension: &str) -> bool {
    matches!(extension, "md" | "mdx" | "rst" | "adoc" | "txt")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_baseline_exception_kind(value: &str) -> bool {
    matches!(
        value,
        "protected" | "generated" | "live-service" | "off-host" | "platform-pending"
    )
}

fn unix_time_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn proof_state_trust_anchor_account(codex_home: &Path, repository_root: &Path) -> String {
    let home_key = repository_state_key(codex_home);
    let repository_key = repository_state_key(repository_root);
    format!("state-v1-{home_key}-{repository_key}")
}

pub(crate) fn completion_proof_trust_anchor_identity_for_tests(
    codex_home: &Path,
    cwd: &Path,
) -> (&'static str, String) {
    let repository_root = canonical_repository_root(cwd);
    (
        TRUST_ANCHOR_SERVICE,
        proof_state_trust_anchor_account(codex_home, &repository_root),
    )
}

pub(crate) fn completion_proof_state_lock_path_for_tests(codex_home: &Path, cwd: &Path) -> PathBuf {
    private_state_lock_path(codex_home, &canonical_repository_root(cwd))
}

fn private_state_lock_path(codex_home: &Path, repository_root: &Path) -> PathBuf {
    codex_home
        .join("completion-proof")
        .join("repositories")
        .join(format!("{}.lock", repository_state_key(repository_root)))
}

async fn load_or_initialize_authenticated_state(
    persistence: &CompletionProofPersistence,
    repository_root: &Path,
) -> Result<(PersistentCompletionProofState, ProofStateIntegrity), String> {
    let state_bytes = match tokio::fs::read(&persistence.state_path).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "the authenticated completion-proof state could not be read: {error}"
            ));
        }
    };
    let anchor_value = load_trust_anchor_value(persistence).await?;

    match (state_bytes, anchor_value) {
        (None, None) => {
            let seal = initialize_trust_anchor(persistence).await?;
            Ok((
                PersistentCompletionProofState::new(repository_root),
                ProofStateIntegrity::Trusted(seal),
            ))
        }
        (Some(_), None) => Err(
            "an unauthenticated or orphaned completion-proof state file was present without its runtime trust anchor"
                .to_string(),
        ),
        (None, Some(anchor_value)) => {
            let (anchor, authentication_key) =
                parse_trust_anchor(persistence, &anchor_value)?;
            if anchor.committed_revision != 0 || anchor.committed_envelope_hash.is_some() {
                return Err(
                    "the authenticated completion-proof state was missing; an older or empty state cannot replace the committed state"
                        .to_string(),
                );
            }
            Ok((
                PersistentCompletionProofState::new(repository_root),
                ProofStateIntegrity::Trusted(ProofStateSeal {
                    key_id: anchor.key_id,
                    authentication_key,
                    revision: 0,
                    envelope_hash: None,
                }),
            ))
        }
        (Some(state_bytes), Some(anchor_value)) => {
            let (persistent, seal) = load_authenticated_state_from_bytes(
                persistence,
                repository_root,
                &state_bytes,
                &anchor_value,
            )
            .await?;
            Ok((persistent, ProofStateIntegrity::Trusted(seal)))
        }
    }
}

async fn load_authenticated_state(
    persistence: &CompletionProofPersistence,
    repository_root: &Path,
) -> Result<(PersistentCompletionProofState, ProofStateSeal), String> {
    let state_bytes = tokio::fs::read(&persistence.state_path)
        .await
        .map_err(|error| {
            format!("the authenticated completion-proof state could not be read: {error}")
        })?;
    let anchor_value = load_trust_anchor_value(persistence).await?.ok_or_else(|| {
        "the runtime trust anchor for completion-proof state was missing".to_string()
    })?;
    load_authenticated_state_from_bytes(persistence, repository_root, &state_bytes, &anchor_value)
        .await
}

async fn load_authenticated_state_from_bytes(
    persistence: &CompletionProofPersistence,
    repository_root: &Path,
    state_bytes: &[u8],
    anchor_value: &str,
) -> Result<(PersistentCompletionProofState, ProofStateSeal), String> {
    let (anchor, authentication_key) = parse_trust_anchor(persistence, anchor_value)?;
    let envelope: AuthenticatedCompletionProofStateV1 = serde_json::from_slice(state_bytes)
        .map_err(|error| {
            format!("the authenticated completion-proof state envelope was invalid: {error}")
        })?;
    if envelope.key_id != anchor.key_id {
        return Err(
            "the authenticated completion-proof state used a different runtime trust-key identity"
                .to_string(),
        );
    }
    validate_authenticated_state_envelope(
        persistence,
        repository_root,
        &envelope,
        &authentication_key,
    )?;
    let envelope_hash = format!("{:x}", Sha256::digest(state_bytes));

    if envelope.revision < anchor.committed_revision {
        return Err(format!(
            "completion-proof state revision {} was replayed behind committed revision {}",
            envelope.revision, anchor.committed_revision
        ));
    }
    if envelope.revision > anchor.committed_revision {
        return Err(format!(
            "completion-proof state revision {} was not committed at protected revision {}",
            envelope.revision, anchor.committed_revision
        ));
    }
    if anchor.committed_envelope_hash.as_deref() != Some(envelope_hash.as_str()) {
        return Err(
            "completion-proof state did not match the exact committed authenticated envelope"
                .to_string(),
        );
    }

    Ok((
        envelope.state,
        ProofStateSeal {
            key_id: anchor.key_id,
            authentication_key,
            revision: envelope.revision,
            envelope_hash: Some(envelope_hash),
        },
    ))
}

async fn initialize_trust_anchor(
    persistence: &CompletionProofPersistence,
) -> Result<ProofStateSeal, String> {
    let mut authentication_key = [0_u8; TRUST_KEY_BYTES];
    rand::rng().fill_bytes(&mut authentication_key);
    let key_id = Uuid::new_v4().to_string();
    let anchor = CompletionProofTrustAnchorV1 {
        schema_version: TRUST_ANCHOR_SCHEMA_VERSION,
        repository_key: persistence.repository_key.clone(),
        key_id: key_id.clone(),
        authentication_key: STANDARD_NO_PAD.encode(authentication_key),
        committed_revision: 0,
        committed_envelope_hash: None,
    };
    save_trust_anchor(persistence, &anchor).await?;
    Ok(ProofStateSeal {
        key_id,
        authentication_key,
        revision: 0,
        envelope_hash: None,
    })
}

async fn persist_authenticated_state(
    persistence: &CompletionProofPersistence,
    persistent: PersistentCompletionProofState,
    seal: &ProofStateSeal,
) -> Result<ProofStateSeal, String> {
    let anchor_value = load_trust_anchor_value(persistence).await?.ok_or_else(|| {
        "the runtime trust anchor disappeared before completion-proof state could be written"
            .to_string()
    })?;
    let (mut anchor, anchor_key) = parse_trust_anchor(persistence, &anchor_value)?;
    if anchor.key_id != seal.key_id
        || anchor_key != seal.authentication_key
        || anchor.committed_revision != seal.revision
        || anchor.committed_envelope_hash != seal.envelope_hash
    {
        return Err(
            "the protected completion-proof high-water mark changed or moved backwards while state was being written"
                .to_string(),
        );
    }

    let revision = seal
        .revision
        .checked_add(1)
        .ok_or_else(|| "completion-proof state revision overflowed".to_string())?;
    let mut envelope = AuthenticatedCompletionProofStateV1 {
        schema_version: AUTHENTICATED_STATE_SCHEMA_VERSION,
        repository_key: persistence.repository_key.clone(),
        key_id: seal.key_id.clone(),
        revision,
        state: persistent,
        authentication_tag: String::new(),
    };
    envelope.authentication_tag = authenticated_state_tag(&envelope, &seal.authentication_key)?;
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|error| {
        format!("the authenticated completion-proof state could not be encoded: {error}")
    })?;
    let envelope_hash = format!("{:x}", Sha256::digest(&bytes));
    // Advance the protected high-water mark before replacing the file. If the file write
    // is interrupted or fails, the older file is thereafter a replay and fails closed.
    anchor.committed_revision = revision;
    anchor.committed_envelope_hash = Some(envelope_hash.clone());
    save_trust_anchor(persistence, &anchor).await?;
    write_private_state(persistence.state_path.clone(), bytes)
        .await
        .map_err(|error| {
            format!("the authenticated completion-proof state could not be committed: {error}")
        })?;

    Ok(ProofStateSeal {
        key_id: seal.key_id.clone(),
        authentication_key: seal.authentication_key,
        revision,
        envelope_hash: Some(envelope_hash),
    })
}

fn validate_authenticated_state_envelope(
    persistence: &CompletionProofPersistence,
    repository_root: &Path,
    envelope: &AuthenticatedCompletionProofStateV1,
    authentication_key: &[u8; TRUST_KEY_BYTES],
) -> Result<(), String> {
    if envelope.schema_version != AUTHENTICATED_STATE_SCHEMA_VERSION
        || envelope.repository_key != persistence.repository_key
        || Uuid::parse_str(&envelope.key_id).is_err()
        || envelope.revision == 0
        || envelope.state.schema_version != STATE_SCHEMA_VERSION
        || !same_completion_proof_path(
            &canonical_repository_root(Path::new(&envelope.state.repository_root)),
            repository_root,
        )
        || !persistent_relaxation_state_is_valid(&envelope.state)
    {
        return Err(
            "the authenticated completion-proof state envelope had the wrong identity or schema"
                .to_string(),
        );
    }
    verify_authenticated_state_tag(envelope, authentication_key)
}

fn persisted_current_user_relaxation_is_valid(relaxation: &PersistedCurrentUserRelaxation) -> bool {
    if valid_exact_identity(relaxation.session_lineage_id.clone()).is_none()
        || relaxation.evidence.is_empty()
        || relaxation.evidence.iter().collect::<BTreeSet<_>>().len() != relaxation.evidence.len()
    {
        return false;
    }
    let provenance_prefix = format!(
        "session:{}/current-user-message:",
        relaxation.session_lineage_id
    );
    let mut decisions = BTreeSet::new();
    for evidence in &relaxation.evidence {
        let Some(message_identity) = evidence.provenance.strip_prefix(&provenance_prefix) else {
            return false;
        };
        if evidence.source != CURRENT_USER_RELAXATION_SOURCE
            || valid_exact_identity(message_identity.to_string()).is_none()
        {
            return false;
        }
        let CompletionProofRelaxationResolution::Clear {
            decision,
            evidence: parsed_evidence,
        } = parse_relaxation_directives(&evidence.text, &evidence.source, &evidence.provenance)
        else {
            return false;
        };
        if parsed_evidence.as_slice() != std::slice::from_ref(evidence) {
            return false;
        }
        decisions.insert(decision);
    }
    match (decisions.len(), relaxation.decision) {
        (1, Some(decision)) => decisions.contains(&decision),
        (2, None) => true,
        _ => false,
    }
}

fn persistent_relaxation_state_is_valid(state: &PersistentCompletionProofState) -> bool {
    let legacy_current_user_is_valid = state
        .current_user_relaxation
        .as_ref()
        .is_none_or(persisted_current_user_relaxation_is_valid);
    let current_users_by_lineage_are_valid =
        state
            .current_user_relaxations_by_lineage
            .iter()
            .all(|(session_lineage_id, relaxation)| {
                valid_exact_identity(session_lineage_id.clone()).is_some()
                    && session_lineage_id == &relaxation.session_lineage_id
                    && persisted_current_user_relaxation_is_valid(relaxation)
            });
    let last_applied_is_valid = state
        .last_applied_relaxation
        .as_ref()
        .is_none_or(|relaxation| {
            matches!(relaxation.authority.as_str(), "current_user" | "repository")
                && valid_exact_identity(relaxation.binding.clone()).is_some()
                && !relaxation.evidence.is_empty()
                && relaxation.evidence.iter().all(|evidence| {
                    valid_exact_identity(evidence.source.clone()).is_some()
                        && valid_exact_identity(evidence.provenance.clone()).is_some()
                        && matches!(
                            parse_relaxation_directives(
                                &evidence.text,
                                &evidence.source,
                                &evidence.provenance,
                            ),
                            CompletionProofRelaxationResolution::Clear {
                                decision:
                                    CompletionProofRelaxationDecision::AllowWithoutCurrentProof,
                                ..
                            }
                        )
                })
        });
    legacy_current_user_is_valid && current_users_by_lineage_are_valid && last_applied_is_valid
}

fn authenticated_state_tag(
    envelope: &AuthenticatedCompletionProofStateV1,
    authentication_key: &[u8; TRUST_KEY_BYTES],
) -> Result<String, String> {
    let payload = authenticated_state_payload(envelope)?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(authentication_key)
        .map_err(|_| "the runtime completion-proof authentication key was invalid".to_string())?;
    mac.update(&payload);
    Ok(STANDARD_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn verify_authenticated_state_tag(
    envelope: &AuthenticatedCompletionProofStateV1,
    authentication_key: &[u8; TRUST_KEY_BYTES],
) -> Result<(), String> {
    let supplied = STANDARD_NO_PAD
        .decode(&envelope.authentication_tag)
        .map_err(|_| {
            "the completion-proof state authentication tag was not valid base64".to_string()
        })?;
    let payload = authenticated_state_payload(envelope)?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(authentication_key)
        .map_err(|_| "the runtime completion-proof authentication key was invalid".to_string())?;
    mac.update(&payload);
    mac.verify_slice(&supplied).map_err(|_| {
        "completion-proof state authentication failed; repository or tool rewriting cannot establish proof"
            .to_string()
    })
}

fn authenticated_state_payload(
    envelope: &AuthenticatedCompletionProofStateV1,
) -> Result<Vec<u8>, String> {
    let mut unsigned = envelope.clone();
    unsigned.authentication_tag.clear();
    serde_json::to_vec(&unsigned).map_err(|error| {
        format!("the completion-proof authentication payload could not be encoded: {error}")
    })
}

fn parse_trust_anchor(
    persistence: &CompletionProofPersistence,
    value: &str,
) -> Result<(CompletionProofTrustAnchorV1, [u8; TRUST_KEY_BYTES]), String> {
    let anchor: CompletionProofTrustAnchorV1 = serde_json::from_str(value).map_err(|error| {
        format!("the runtime completion-proof trust anchor was invalid: {error}")
    })?;
    if anchor.schema_version != TRUST_ANCHOR_SCHEMA_VERSION
        || anchor.repository_key != persistence.repository_key
        || Uuid::parse_str(&anchor.key_id).is_err()
        || (anchor.committed_revision == 0) != anchor.committed_envelope_hash.is_none()
        || anchor
            .committed_envelope_hash
            .as_ref()
            .is_some_and(|hash| !is_sha256(hash))
    {
        return Err(
            "the runtime completion-proof trust anchor had the wrong identity, schema, or high-water mark"
                .to_string(),
        );
    }
    let decoded = STANDARD_NO_PAD
        .decode(&anchor.authentication_key)
        .map_err(|_| "the runtime completion-proof trust key was not valid base64".to_string())?;
    let authentication_key: [u8; TRUST_KEY_BYTES] = decoded.try_into().map_err(|_| {
        format!("the runtime completion-proof trust key was not {TRUST_KEY_BYTES} bytes")
    })?;
    Ok((anchor, authentication_key))
}

async fn load_trust_anchor_value(
    persistence: &CompletionProofPersistence,
) -> Result<Option<String>, String> {
    let store = Arc::clone(&persistence.trust_store);
    let account = persistence.trust_anchor_account.clone();
    tokio::task::spawn_blocking(move || {
        store
            .load(TRUST_ANCHOR_SERVICE, &account)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("the runtime trust-anchor task failed: {error}"))?
    .map_err(|error| format!("the runtime trust anchor could not be read: {error}"))
}

async fn save_trust_anchor(
    persistence: &CompletionProofPersistence,
    anchor: &CompletionProofTrustAnchorV1,
) -> Result<(), String> {
    let value = serde_json::to_string(anchor)
        .map_err(|error| format!("the runtime trust anchor could not be encoded: {error}"))?;
    let store = Arc::clone(&persistence.trust_store);
    let account = persistence.trust_anchor_account.clone();
    tokio::task::spawn_blocking(move || {
        store
            .save(TRUST_ANCHOR_SERVICE, &account, &value)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("the runtime trust-anchor task failed: {error}"))?
    .map_err(|error| format!("the runtime trust anchor could not be committed: {error}"))
}

async fn write_private_state(state_path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
    tokio::task::spawn_blocking(move || write_private_state_blocking(&state_path, &bytes))
        .await
        .map_err(|error| io::Error::other(format!("completion proof state task failed: {error}")))?
}

struct PrivateStateFileLock {
    file: std::fs::File,
}

impl Drop for PrivateStateFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

async fn acquire_private_state_lock(lock_path: PathBuf) -> io::Result<PrivateStateFileLock> {
    let deadline = tokio::time::Instant::now() + PRIVATE_STATE_LOCK_WAIT;
    let open_task = tokio::task::spawn_blocking(move || {
        let parent = lock_path
            .parent()
            .ok_or_else(|| io::Error::other("completion proof lock path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
    });
    let file = match tokio::time::timeout_at(deadline, open_task).await {
        Ok(result) => result.map_err(|error| {
            io::Error::other(format!("completion proof lock task failed: {error}"))
        })??,
        Err(_) => return Err(private_state_lock_timeout_error()),
    };

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(private_state_lock_timeout_error());
        }
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(PrivateStateFileLock { file }),
            Err(error) if is_private_state_lock_contended(&error) => {}
            Err(error) => return Err(error),
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(private_state_lock_timeout_error());
        }
        tokio::time::sleep(PRIVATE_STATE_LOCK_RETRY_INTERVAL.min(deadline - now)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(private_state_lock_timeout_error());
        }
    }
}

fn is_private_state_lock_contended(error: &io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error().is_some() && error.raw_os_error() == contended.raw_os_error()
}

fn private_state_lock_timeout_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "timed out after 10 seconds waiting for the private completion-proof state lock",
    )
}

fn write_private_state_blocking(state_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = state_path
        .parent()
        .ok_or_else(|| io::Error::other("completion proof state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(state_path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;

    fn test_artifact(repository_root: &Path) -> CompletionProofArtifactV1 {
        CompletionProofArtifactV1 {
            schema_version: 1,
            artifact_type: "CompletionProofArtifactV1".to_string(),
            policy_id: "test-policy".to_string(),
            policy_runner_bundle_sha256: "f".repeat(64),
            exact_command: "test completion-proof".to_string(),
            repository_root: repository_root.to_string_lossy().into_owned(),
            host_identity: HostIdentity {
                hostname: "test-host".to_string(),
                system: "test-system".to_string(),
                release: "test-release".to_string(),
                machine: "test-machine".to_string(),
            },
            invocation_nonce: "test-nonce".to_string(),
            parent_pid: 1,
            observed_runner_parent_pid: 1,
            start_fingerprint: "a".repeat(64),
            end_fingerprint: "a".repeat(64),
            workspace_fingerprint: "a".repeat(64),
            start_mutation_epoch: 1,
            end_mutation_epoch: 1,
            mutation_epoch: 1,
            inventory_hash: "b".repeat(64),
            report_hash: "c".repeat(64),
            attempt_report_hash: "d".repeat(64),
            attempt_id: Uuid::new_v4().to_string(),
            validations: Vec::new(),
            child_processes: Vec::new(),
            exceptions: Vec::new(),
            overrides: Vec::new(),
            registered_at_unix_ms: 1,
            artifact_hash: "e".repeat(64),
        }
    }

    async fn test_ledger(
        codex_home: &Path,
        repository_root: &Path,
        trust_store: Arc<MockKeyringStore>,
    ) -> CompletionProofLedger {
        let trust_store: Arc<dyn KeyringStore> = trust_store;
        let authority = CompletionProofSessionAuthority::root_terminal_owner(
            CompletionProofRuntimeRegistry::new(),
            repository_root,
        );
        CompletionProofLedger::load_or_new_with_trust_store(
            codex_home.to_path_buf(),
            repository_root,
            authority,
            trust_store,
        )
        .await
    }

    async fn test_ledger_for_session(
        codex_home: &Path,
        repository_root: &Path,
        session_lineage_id: &str,
        trust_store: Arc<MockKeyringStore>,
    ) -> CompletionProofLedger {
        let trust_store: Arc<dyn KeyringStore> = trust_store;
        let authority = CompletionProofSessionAuthority::root_terminal_owner(
            CompletionProofRuntimeRegistry::new(),
            repository_root,
        );
        CompletionProofLedger::load_or_new_with_context_and_trust_store(
            codex_home.to_path_buf(),
            repository_root,
            authority,
            Some(session_lineage_id.to_string()),
            trust_store,
        )
        .await
    }

    async fn record_non_documentation_mutation(
        ledger: &CompletionProofLedger,
        repository_root: &Path,
        relative_path: &str,
    ) {
        ledger
            .note_mutation_paths(
                repository_root,
                Some(&BTreeSet::from([PathBuf::from(relative_path)])),
            )
            .await;
    }

    fn run_git(repository_root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repository_root)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_all(repository_root: &Path, message: &str) {
        run_git(repository_root, &["add", "."]);
        run_git(
            repository_root,
            &[
                "-c",
                disabled_hooks_argument(),
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Codex Test",
                "-c",
                "user.email=codex-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
    }

    #[test]
    fn compiled_kd4_authority_trusts_both_native_validation_adapters() {
        let authority = compiled_kd4_authority().expect("load compiled KD4 authority");

        for validation_id in [
            "tools.argument-comment-lint.native",
            "windows.sandbox-smoke",
        ] {
            let contract = authority
                .config
                .validation_evidence_contracts
                .get(validation_id)
                .unwrap_or_else(|| panic!("missing native validation {validation_id}"));
            assert_eq!(
                contract.evidence_kind,
                ValidationEvidenceKind::StructuredTest
            );
            assert_eq!(contract.validation_type, None);
        }

        let trusted_paths = KD4_TRUSTED_BUNDLE_MEMBERS
            .iter()
            .map(|member| member.relative_path)
            .collect::<BTreeSet<_>>();
        assert!(trusted_paths.contains("tools/argument-comment-lint/native_test_runner.py"));
        assert!(trusted_paths.contains("codex-rs/windows-sandbox-rs/sandbox_smoketests.py"));
    }

    #[test]
    fn compiled_kd4_rust_workspace_validation_consumes_investigation_evidence_schema() {
        let authority = compiled_kd4_authority().expect("load compiled KD4 authority");
        let input_contract = authority
            .config
            .validation_path_patterns
            .get("rust.nextest.workspace")
            .expect("missing Rust workspace validation path contract");

        assert!(
            input_contract
                .content_paths
                .contains("docs/schemas/investigation-evidence-v1.schema.json")
        );
    }

    #[test]
    fn workspace_fingerprint_binds_head_identity() {
        let worktree_identity = "f".repeat(64);
        let first = completion_workspace_fingerprint(Some("first-head"), &worktree_identity);
        let second = completion_workspace_fingerprint(Some("second-head"), &worktree_identity);
        let unborn = completion_workspace_fingerprint(None, &worktree_identity);

        assert_ne!(first, second);
        assert_ne!(first, unborn);
        assert_eq!(first.len(), 64);
    }

    #[tokio::test]
    async fn python_and_runtime_workspace_fingerprints_match_for_real_git_repository() {
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::create_dir_all(repository.path().join("src")).expect("create source directory");
        std::fs::write(
            repository.path().join("src/runtime.rs"),
            "const VALUE: u8 = 1;\n",
        )
        .expect("write tracked source");
        commit_all(repository.path(), "initial");
        std::fs::write(
            repository.path().join("src/runtime.rs"),
            "const VALUE: u8 = 2;\n",
        )
        .expect("write dirty source");

        let runtime = workspace_observation(repository.path())
            .await
            .expect("runtime workspace observation")
            .fingerprint;
        let script_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/completion_proof.py");
        let python = if cfg!(windows) { "python" } else { "python3" };
        let output = std::process::Command::new(python)
            .args([
                "-c",
                concat!(
                    "import importlib.util, pathlib, sys; ",
                    "spec = importlib.util.spec_from_file_location('kd4_completion_proof', sys.argv[1]); ",
                    "module = importlib.util.module_from_spec(spec); ",
                    "sys.modules[spec.name] = module; ",
                    "spec.loader.exec_module(module); ",
                    "print(module.workspace_fingerprint(pathlib.Path(sys.argv[2])))",
                ),
            ])
            .arg(&script_path)
            .arg(repository.path())
            .output()
            .expect("run Python workspace fingerprint");
        assert!(
            output.status.success(),
            "Python workspace fingerprint failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let python = String::from_utf8(output.stdout)
            .expect("Python fingerprint is UTF-8")
            .trim()
            .to_string();

        assert_eq!(runtime, python);
    }

    #[test]
    fn porcelain_v2_workspace_paths_preserve_rename_source_and_fail_closed() {
        assert_eq!(
            strict_nul_separated_paths(b"docs/readme.md\0src/runtime.rs\0"),
            Some(BTreeSet::from([
                "docs/readme.md".to_string(),
                "src/runtime.rs".to_string(),
            ]))
        );
        for malformed in [
            b"src/runtime.rs".as_slice(),
            b"\0".as_slice(),
            b"src/runtime.rs\0\0".as_slice(),
            b"src/runtime.rs\0src/runtime.rs\0".as_slice(),
            b"./src/runtime.rs\0".as_slice(),
            b".GIT/private-state\0".as_slice(),
            b"\xff\0".as_slice(),
        ] {
            assert_eq!(strict_nul_separated_paths(malformed), None);
        }

        let status = format!(
            "2 R. N... 100644 100644 100644 {} {} R100 docs/runtime.rs\0src/runtime.rs\0",
            "a".repeat(40),
            "b".repeat(40),
        );
        assert_eq!(
            workspace_generation_paths(status.as_bytes()),
            Some(vec![
                "docs/runtime.rs".to_string(),
                "src/runtime.rs".to_string(),
            ])
        );

        for malformed in [
            b"x unknown\0".as_slice(),
            b"? unterminated.txt".as_slice(),
            b"2 R. N... 100644 100644 100644 a b R100 docs/runtime.rs\0".as_slice(),
            b"? ../outside.txt\0".as_slice(),
            b"? ./src/runtime.rs\0".as_slice(),
            b"? .git/private-state\0".as_slice(),
            b"? .GIT/private-state\0".as_slice(),
            b"? \xff\0".as_slice(),
        ] {
            assert_eq!(workspace_generation_paths(malformed), None);
        }
    }

    #[tokio::test]
    async fn current_user_relaxation_reaches_the_gate_and_is_session_scoped() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session-a",
            Arc::clone(&trust_store),
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;

        assert!(matches!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Blocked { .. }
        ));
        let exact_instruction = "- completion-proof: allow completion without current proof.";
        assert!(
            ledger
                .observe_current_user_completion_proof_instruction(
                    &format!("Please use this exact policy:\n{exact_instruction}"),
                    "turn-7/message-2",
                )
                .await
                .expect("record explicit user instruction")
        );
        assert_eq!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Accepted
        );
        {
            let state = ledger.state.lock().await;
            assert!(
                state.persistent.current_user_relaxation.is_none(),
                "new directives must not write the legacy singleton"
            );
            let relaxation = state
                .persistent
                .current_user_relaxations_by_lineage
                .get("root-session-a")
                .expect("persisted current-user relaxation");
            assert_eq!(relaxation.session_lineage_id, "root-session-a");
            assert_eq!(
                relaxation.decision,
                Some(CompletionProofRelaxationDecision::AllowWithoutCurrentProof)
            );
            assert_eq!(relaxation.evidence[0].text, exact_instruction);
            assert_eq!(
                relaxation.evidence[0].source,
                CURRENT_USER_RELAXATION_SOURCE
            );
            assert_eq!(
                relaxation.evidence[0].provenance,
                "session:root-session-a/current-user-message:turn-7/message-2"
            );
            let applied = state
                .persistent
                .last_applied_relaxation
                .as_ref()
                .expect("gate records the exact applied evidence");
            assert_eq!(applied.authority, "current_user");
            assert_eq!(applied.evidence, relaxation.evidence);
        }
        drop(ledger);

        let resumed = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session-a",
            Arc::clone(&trust_store),
        )
        .await;
        assert_eq!(
            resumed.check_gate().await,
            CompletionProofGateDecision::Accepted,
            "the same root-session lineage must retain authenticated user provenance"
        );
        drop(resumed);

        let unrelated = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session-b",
            trust_store,
        )
        .await;
        assert!(
            matches!(
                unrelated.check_gate().await,
                CompletionProofGateDecision::Blocked { .. }
            ),
            "an unrelated session must not replay another user's signed relaxation"
        );
    }

    #[tokio::test]
    async fn current_user_requirement_wins_over_repository_relaxation() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::write(
            repository.path().join("AGENTS.md"),
            format!("# Policy\n\n- {RELAXATION_ALLOW_DIRECTIVE}.\n"),
        )
        .expect("write repository instruction");
        std::fs::write(repository.path().join("behavior.rs"), "initial\n")
            .expect("write initial behavior");
        commit_all(repository.path(), "initial");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session",
            trust_store,
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;
        assert_eq!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Accepted,
            "a clear unchanged committed repository instruction may relax proof"
        );

        assert!(
            ledger
                .observe_current_user_completion_proof_instruction(
                    RELAXATION_REQUIRE_DIRECTIVE,
                    "turn-8",
                )
                .await
                .expect("record current-user requirement")
        );
        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("the current user's requirement must override repository relaxation");
        };
        assert!(message.contains("current user explicitly required"));
        assert!(
            ledger
                .state
                .lock()
                .await
                .persistent
                .last_applied_relaxation
                .is_none()
        );
    }

    #[tokio::test]
    async fn ambiguous_current_user_instruction_fails_closed_over_repository_permission() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::write(
            repository.path().join("AGENTS.md"),
            format!("{RELAXATION_ALLOW_DIRECTIVE}\n"),
        )
        .expect("write repository instruction");
        std::fs::write(repository.path().join("behavior.rs"), "initial\n")
            .expect("write initial behavior");
        commit_all(repository.path(), "initial");
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session",
            Arc::new(MockKeyringStore::default()),
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;
        assert!(
            ledger
                .observe_current_user_completion_proof_instruction(
                    &format!("{RELAXATION_ALLOW_DIRECTIVE}\n{RELAXATION_REQUIRE_DIRECTIVE}"),
                    "turn-9",
                )
                .await
                .expect("record ambiguous current-user instruction")
        );

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("ambiguous current-user input must fail closed");
        };
        assert!(message.contains("current-user completion-proof instruction was ambiguous"));
        let state = ledger.state.lock().await;
        assert!(
            state.persistent.current_user_relaxation.is_none(),
            "new directives must not write the legacy singleton"
        );
        let relaxation = state
            .persistent
            .current_user_relaxations_by_lineage
            .get("root-session")
            .expect("persist ambiguity and its exact evidence");
        assert_eq!(relaxation.decision, None);
        assert_eq!(relaxation.evidence.len(), 2);
    }

    #[tokio::test]
    async fn ambiguous_repository_instruction_fails_closed_through_the_gate() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::write(
            repository.path().join("AGENTS.md"),
            format!("{RELAXATION_ALLOW_DIRECTIVE}\n{RELAXATION_REQUIRE_DIRECTIVE}\n"),
        )
        .expect("write ambiguous repository instruction");
        std::fs::write(repository.path().join("behavior.rs"), "initial\n")
            .expect("write initial behavior");
        commit_all(repository.path(), "initial");
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session",
            Arc::new(MockKeyringStore::default()),
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("conflicting repository instructions must fail closed");
        };
        assert!(message.contains("Repository completion-proof instructions were ambiguous"));
    }

    #[test]
    fn directive_examples_are_not_explicit_relaxation_instructions() {
        let text = format!(
            "Here is an example:\n```text\n{RELAXATION_ALLOW_DIRECTIVE}\n```\n\n    {RELAXATION_ALLOW_DIRECTIVE}\n\n> {RELAXATION_ALLOW_DIRECTIVE}\n"
        );
        assert!(matches!(
            parse_relaxation_directives(&text, "AGENTS.md", "repository:HEAD:abc:AGENTS.md"),
            CompletionProofRelaxationResolution::None
        ));
    }

    #[tokio::test]
    async fn repository_relaxation_rejects_non_instruction_files_and_later_rewrites() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::write(
            repository.path().join("README.md"),
            format!("{RELAXATION_ALLOW_DIRECTIVE}\n"),
        )
        .expect("write look-alike prose");
        std::fs::write(repository.path().join("behavior.rs"), "initial\n")
            .expect("write initial behavior");
        commit_all(repository.path(), "initial");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session",
            Arc::clone(&trust_store),
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;
        assert!(matches!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Blocked { .. }
        ));
        drop(ledger);

        std::fs::write(
            repository.path().join("AGENTS.md"),
            format!("{RELAXATION_ALLOW_DIRECTIVE}\n"),
        )
        .expect("write repository instruction");
        commit_all(repository.path(), "add explicit instruction");
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "root-session",
            trust_store,
        )
        .await;
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;
        assert_eq!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Accepted
        );

        std::fs::write(
            repository.path().join("AGENTS.md"),
            format!("# rewritten by a tool\n{RELAXATION_ALLOW_DIRECTIVE}\n"),
        )
        .expect("rewrite instruction after capture");
        record_non_documentation_mutation(&ledger, repository.path(), "AGENTS.md").await;
        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("a later instruction rewrite must invalidate repository permission");
        };
        assert!(message.contains("instructions changed after their provenance was captured"));
    }

    #[tokio::test]
    async fn gate_observes_a_clean_head_change_through_the_runtime_path() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        run_git(repository.path(), &["init", "--quiet"]);
        std::fs::write(repository.path().join("behavior.rs"), "first\n")
            .expect("write first revision");
        run_git(repository.path(), &["add", "behavior.rs"]);
        run_git(
            repository.path(),
            &[
                "-c",
                disabled_hooks_argument(),
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Codex Test",
                "-c",
                "user.email=codex-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "first",
            ],
        );
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger(codex_home.path(), repository.path(), trust_store).await;
        assert_eq!(
            ledger.check_gate().await,
            CompletionProofGateDecision::Accepted
        );

        std::fs::write(repository.path().join("behavior.rs"), "second\n")
            .expect("write second revision");
        run_git(repository.path(), &["add", "behavior.rs"]);
        run_git(
            repository.path(),
            &[
                "-c",
                disabled_hooks_argument(),
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Codex Test",
                "-c",
                "user.email=codex-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "second",
            ],
        );

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("a clean HEAD change must stale completion state");
        };
        assert!(message.contains("non-documentation changes"));
    }

    #[test]
    fn mutation_paths_union_explicit_and_observed_changes() {
        let explicit = vec!["codex-rs/core/src/lib.rs".to_string()];
        let observed = vec![
            "codex-rs/core/src/lib.rs".to_string(),
            "codex-rs/core/src/other.rs".to_string(),
        ];

        assert_eq!(
            union_mutation_paths(Some(&explicit), Some(&observed)),
            Some(vec![
                "codex-rs/core/src/lib.rs".to_string(),
                "codex-rs/core/src/other.rs".to_string(),
            ])
        );
    }

    #[tokio::test]
    async fn authenticated_legacy_singleton_remains_exact_lineage_scoped_without_migration() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "legacy-root-session",
            Arc::clone(&trust_store),
        )
        .await;
        std::fs::write(repository.path().join("behavior.rs"), "changed\n")
            .expect("write behavior mutation");
        record_non_documentation_mutation(&ledger, repository.path(), "behavior.rs").await;

        let provenance =
            "session:legacy-root-session/current-user-message:legacy-turn-1".to_string();
        let CompletionProofRelaxationResolution::Clear { decision, evidence } =
            parse_relaxation_directives(
                RELAXATION_ALLOW_DIRECTIVE,
                CURRENT_USER_RELAXATION_SOURCE,
                &provenance,
            )
        else {
            panic!("legacy fixture directive must parse clearly");
        };
        let legacy_relaxation = PersistedCurrentUserRelaxation {
            session_lineage_id: "legacy-root-session".to_string(),
            decision: Some(decision),
            evidence,
        };

        let state_file_lock = acquire_private_state_lock(ledger.persistence.lock_path.clone())
            .await
            .expect("state lock");
        {
            let mut state = ledger.state.lock().await;
            assert!(
                state
                    .persistent
                    .current_user_relaxations_by_lineage
                    .is_empty()
            );
            state.persistent.current_user_relaxation = Some(legacy_relaxation.clone());
        }
        ledger
            .persist()
            .await
            .expect("persist authenticated legacy singleton");
        drop(state_file_lock);

        let legacy_bytes = tokio::fs::read(&ledger.persistence.state_path)
            .await
            .expect("read authenticated legacy singleton");
        let legacy_json: serde_json::Value =
            serde_json::from_slice(&legacy_bytes).expect("parse authenticated legacy singleton");
        assert!(
            legacy_json["state"]
                .get("current_user_relaxations_by_lineage")
                .is_none(),
            "an empty keyed map must be omitted so legacy authentication bytes stay compatible"
        );
        drop(ledger);

        let matching = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "legacy-root-session",
            Arc::clone(&trust_store),
        )
        .await;
        {
            let state = matching.state.lock().await;
            assert_eq!(
                state.persistent.current_user_relaxation.as_ref(),
                Some(&legacy_relaxation)
            );
            assert!(
                state
                    .persistent
                    .current_user_relaxations_by_lineage
                    .is_empty()
            );
        }
        assert_eq!(
            matching.check_gate().await,
            CompletionProofGateDecision::Accepted,
            "the authenticated legacy singleton must remain valid for its exact lineage"
        );
        drop(matching);

        let unrelated = test_ledger_for_session(
            codex_home.path(),
            repository.path(),
            "unrelated-root-session",
            trust_store,
        )
        .await;
        assert!(
            matches!(
                unrelated.check_gate().await,
                CompletionProofGateDecision::Blocked { .. }
            ),
            "an unrelated lineage must not inherit the authenticated legacy singleton"
        );
    }

    #[tokio::test]
    async fn gate_rejects_repository_rewrite_of_authenticated_proof_state() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger(codex_home.path(), repository.path(), trust_store).await;

        let state_file_lock = acquire_private_state_lock(ledger.persistence.lock_path.clone())
            .await
            .expect("state lock");
        {
            let mut state = ledger.state.lock().await;
            state.persistent.mutation_epoch = 1;
            state.persistent.requires_non_documentation_proof = true;
            state.persistent.registered_proof = Some(test_artifact(&ledger.repository_root));
        }
        ledger.persist().await.expect("persist signed state");
        drop(state_file_lock);

        let bytes = tokio::fs::read(&ledger.persistence.state_path)
            .await
            .expect("read signed state");
        let mut envelope: AuthenticatedCompletionProofStateV1 =
            serde_json::from_slice(&bytes).expect("parse signed state");
        envelope.state.requires_non_documentation_proof = false;
        envelope
            .state
            .registered_proof
            .as_mut()
            .expect("registered proof")
            .exact_command = "forged completion-proof".to_string();
        tokio::fs::write(
            &ledger.persistence.state_path,
            serde_json::to_vec_pretty(&envelope).expect("encode rewritten state"),
        )
        .await
        .expect("rewrite state as repository tool");

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("rewritten authenticated state must block terminal completion");
        };
        assert!(message.contains("could not be authenticated"));
        assert!(message.contains("authentication failed"));
    }

    #[tokio::test]
    async fn gate_rejects_replayed_older_authenticated_state() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger(codex_home.path(), repository.path(), trust_store).await;
        let older_state = tokio::fs::read(&ledger.persistence.state_path)
            .await
            .expect("read older signed state");

        let state_file_lock = acquire_private_state_lock(ledger.persistence.lock_path.clone())
            .await
            .expect("state lock");
        {
            let mut state = ledger.state.lock().await;
            state.persistent.mutation_epoch = 1;
            state.persistent.requires_non_documentation_proof = true;
        }
        ledger.persist().await.expect("persist newer signed state");
        drop(state_file_lock);
        tokio::fs::write(&ledger.persistence.state_path, older_state)
            .await
            .expect("replay older signed state");

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("replayed authenticated state must block terminal completion");
        };
        assert!(message.contains("could not be authenticated"));
        assert!(message.contains("replayed behind committed revision"));
    }

    #[tokio::test]
    async fn authenticated_requirement_survives_runtime_reload() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let first = test_ledger(
            codex_home.path(),
            repository.path(),
            Arc::clone(&trust_store),
        )
        .await;
        let state_file_lock = acquire_private_state_lock(first.persistence.lock_path.clone())
            .await
            .expect("state lock");
        {
            let mut state = first.state.lock().await;
            state.persistent.mutation_epoch = 1;
            state.persistent.requires_non_documentation_proof = true;
        }
        first.persist().await.expect("persist required proof state");
        drop(state_file_lock);
        drop(first);

        let resumed = test_ledger(codex_home.path(), repository.path(), trust_store).await;
        let CompletionProofGateDecision::Blocked { message } = resumed.check_gate().await else {
            panic!("reloaded runtime must preserve the proof requirement");
        };
        assert!(message.contains("non-documentation changes"));
        assert!(!message.contains("could not be authenticated"));
    }

    #[tokio::test]
    async fn failed_state_write_makes_older_file_a_replay_after_restart() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger(
            codex_home.path(),
            repository.path(),
            Arc::clone(&trust_store),
        )
        .await;
        let older_state = tokio::fs::read(&ledger.persistence.state_path)
            .await
            .expect("read older signed state");
        tokio::fs::remove_file(&ledger.persistence.state_path)
            .await
            .expect("remove state before obstruction");
        tokio::fs::create_dir(&ledger.persistence.state_path)
            .await
            .expect("obstruct state-file replacement");

        let state_file_lock = acquire_private_state_lock(ledger.persistence.lock_path.clone())
            .await
            .expect("state lock");
        ledger
            .state
            .lock()
            .await
            .persistent
            .requires_non_documentation_proof = true;
        let error = ledger
            .persist()
            .await
            .expect_err("obstructed state write must fail");
        assert!(error.contains("could not be committed"));
        drop(state_file_lock);
        tokio::fs::remove_dir(&ledger.persistence.state_path)
            .await
            .expect("remove state-file obstruction");
        tokio::fs::write(&ledger.persistence.state_path, older_state)
            .await
            .expect("restore older signed state");
        drop(ledger);

        let resumed = test_ledger(codex_home.path(), repository.path(), trust_store).await;
        let CompletionProofGateDecision::Blocked { message } = resumed.check_gate().await else {
            panic!("an older file must not survive a failed newer write");
        };
        assert!(message.contains("could not be authenticated"));
        assert!(message.contains("replayed behind committed revision"));
    }

    #[tokio::test]
    async fn unavailable_state_lock_blocks_runtime_gate() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        let repository = tempfile::tempdir().expect("temporary repository");
        std::fs::write(
            codex_home.path().join("completion-proof"),
            b"not a directory",
        )
        .expect("create lock-path obstruction");
        let trust_store = Arc::new(MockKeyringStore::default());
        let ledger = test_ledger(codex_home.path(), repository.path(), trust_store).await;

        let CompletionProofGateDecision::Blocked { message } = ledger.check_gate().await else {
            panic!("an unavailable state lock must block terminal completion");
        };
        assert!(message.contains("state lock is unavailable"));
    }
}
