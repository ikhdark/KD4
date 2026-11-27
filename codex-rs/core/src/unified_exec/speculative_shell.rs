//! Speculative PowerShell 7 host for local, non-interactive `-Command` launches.
//!
//! Starting pwsh costs roughly 150 ms before a command runs. After an eligible
//! launch (and when a shell-capable call is announced), bootstrap hosts are
//! pre-started with the same executable, flags, working directory and environment.
//! When a later launch has the identical key, its exact `-Command` script is handed
//! to such a host over a private named pipe.
//!
//! Spares live in one process-wide pool (at most two hosts, up to two per key so
//! back-to-back launches find a host that had time to initialize) shared by every
//! thread in an app-server. `CODEX_THREAD_ID` is the only per-thread variable, so it
//! is excluded from the key and bound by the bootstrap at handoff, before caller code.
//!
//! A host is created like an ordinary launch, then lowered to below normal while
//! still suspended so its startup yields to demand work; the bootstrap restores the
//! class Windows assigned at creation before running caller code.
//!
//! The bootstrap (`speculative_shell_bootstrap.ps1`) always starts with `-NoProfile`,
//! so a discarded speculation never runs user profile code. For a login launch it
//! accepts only when every `$PROFILE` file is absent or empty, which makes skipping
//! profile loading equivalent; once it reports a non-empty profile, no further login
//! hosts are prepared in this process. It accepts only scripts that parse and admit an
//! appended `$?` capture, dot-sources the script at global scope and leaves the exit
//! code to the host's `-Command` rule (`$?` of the last statement, or an explicit
//! `exit`). Any key mismatch, decline, timeout, dead host or failure falls back to
//! the ordinary launch; an unused bootstrap is terminated before running caller code.
//!
//! The script is written only after the pipe client is verified to be the host's
//! own process. An accepted host is attributed to the consuming call's process
//! observation scope as its requested root (creation time kept, hand-off time added).
//!
//! Disabled by default on Windows; set `KD4_SPECULATIVE_SHELL=1` to opt in.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
#[cfg(windows)]
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::AtomicU64;
#[cfg(windows)]
use std::sync::atomic::Ordering;
#[cfg(windows)]
use std::time::Duration;

use codex_utils_pty::SpawnedProcess;

/// Names the private pipe; the bootstrap removes it before caller code runs.
const BOOTSTRAP_PIPE_ENV: &str = "KD4_SPECULATIVE_SHELL_PIPE";
/// Marks a login launch; the bootstrap removes it before caller code runs.
const BOOTSTRAP_LOGIN_ENV: &str = "KD4_SPECULATIVE_SHELL_LOGIN";
/// Per-thread variable bound at handoff so spares can serve any thread.
const LATE_BOUND_ENV: &str = "CODEX_THREAD_ID";
const ENABLE_ENV: &str = "KD4_SPECULATIVE_SHELL";
/// Process-wide bound on idle speculative hosts.
#[cfg(windows)]
const POOL_CAP: usize = 2;
/// Spares kept per key, so back-to-back launches each find a host started at
/// least two launches earlier rather than one still initializing.
#[cfg(windows)]
const SPARES_PER_KEY: usize = 2;
#[cfg(windows)]
const IDLE_LIMIT: Duration = Duration::from_secs(45);
#[cfg(windows)]
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);

/// Screened one-line bootstrap. It warms the parser, connects to the private pipe,
/// reads `thread-id \n priority-class \n byte-length \n script`, and replies `A`
/// (accepted, priority restored, about to run), `D` (declined, nothing run) or `P`
/// (login launch declined because a profile is not empty, nothing run).
#[cfg(windows)]
const BOOTSTRAP: &str = include_str!("speculative_shell_bootstrap.ps1");

fn enabled() -> bool {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var(ENABLE_ENV).is_ok_and(|value| value != "0"));
    *ENABLED
}

/// Everything that shapes a launch except the script and the late-bound thread id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpeculativeShellKey {
    program: String,
    flags: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    arg0: Option<String>,
}

/// One eligible launch: its key, exact script and late-bound thread id.
pub(crate) struct SpeculativeLaunch<'a> {
    pub(crate) key: SpeculativeShellKey,
    script: &'a str,
    thread_id: String,
}

impl SpeculativeShellKey {
    /// Accepts only the inline pwsh `-Command` forms produced by `Shell::derive_exec_args`.
    pub(crate) fn for_launch<'a>(
        command: &'a [String],
        cwd: &Path,
        env: &HashMap<String, String>,
        arg0: &Option<String>,
    ) -> Option<SpeculativeLaunch<'a>> {
        if !enabled() {
            return None;
        }
        let (program, rest) = command.split_first()?;
        let (script, flags) = rest.split_last()?;
        let name = Path::new(program).file_name()?.to_str()?.to_ascii_lowercase();
        if !matches!(name.as_str(), "pwsh.exe" | "pwsh") {
            return None;
        }
        let lowered = flags
            .iter()
            .map(|flag| flag.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let lowered = lowered.iter().map(String::as_str).collect::<Vec<_>>();
        if !matches!(
            lowered.as_slice(),
            ["-noninteractive", "-command"] | ["-noprofile", "-noninteractive", "-command"]
        ) || script == "-"
            || env.keys().any(|key| {
                key.eq_ignore_ascii_case(BOOTSTRAP_PIPE_ENV)
                    || key.eq_ignore_ascii_case(BOOTSTRAP_LOGIN_ENV)
            })
        {
            return None;
        }
        let mut thread_ids = env
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(LATE_BOUND_ENV));
        let thread_id = match (thread_ids.next(), thread_ids.next()) {
            (None, _) => String::new(),
            // Exactly one, canonically named, single-line value can be late-bound.
            (Some((key, value)), None)
                if key == LATE_BOUND_ENV && !value.is_empty() && !value.contains(['\r', '\n']) =>
            {
                value.clone()
            }
            _ => return None,
        };
        Some(SpeculativeLaunch {
            key: Self {
                program: program.clone(),
                flags: flags.to_vec(),
                cwd: cwd.to_path_buf(),
                env: env
                    .iter()
                    .filter(|(key, _)| !key.eq_ignore_ascii_case(LATE_BOUND_ENV))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                arg0: arg0.clone(),
            },
            script: script.as_str(),
            thread_id,
        })
    }

    #[cfg(windows)]
    fn is_login(&self) -> bool {
        !self
            .flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("-noprofile"))
    }
}

#[cfg(windows)]
struct Prepared {
    key: SpeculativeShellKey,
    process: SpawnedProcess,
    /// The host's OS process id; the only client allowed to receive the script.
    pid: u32,
    /// Start record, adopted by the consuming call's scope on acceptance.
    observation: codex_utils_pty::process_observation::ObservedProcess,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    generation: u64,
    started: std::time::Instant,
    /// Priority class to restore before caller code; 0 when the host was not lowered.
    restore_class: u32,
}

/// Process-wide idle spares, oldest first.
#[cfg(windows)]
static POOL: LazyLock<tokio::sync::Mutex<Vec<Prepared>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(Vec::new()));
#[cfg(windows)]
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Set when a login launch was declined for a non-empty profile: every later login
/// host would decline the same way, so none is prepared again in this process.
#[cfg(windows)]
static LOGIN_PROFILE_DECLINED: AtomicBool = AtomicBool::new(false);

/// Dropping a host handle terminates its job synchronously; keep that off async
/// workers and outside the pool lock.
#[cfg(windows)]
fn retire<T: Send + 'static>(value: T) {
    drop(tokio::task::spawn_blocking(move || drop(value)));
}

/// Per-session lookahead state; spares themselves are process-wide.
#[derive(Default)]
pub(crate) struct SpeculativeShellSlot {
    last_key: StdMutex<Option<SpeculativeShellKey>>,
}

impl SpeculativeShellSlot {
    /// Hands the launch's script to a matching spare. `None` means launch normally.
    #[cfg(windows)]
    pub(crate) async fn take_and_hand_off(
        &self,
        launch: &SpeculativeLaunch<'_>,
    ) -> Option<SpawnedProcess> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let prepared = {
            let mut pool = POOL.lock().await;
            let index = pool.iter().position(|prepared| prepared.key == launch.key)?;
            pool.remove(index)
        };
        let Prepared {
            process,
            pid,
            observation,
            mut pipe,
            started,
            restore_class,
            ..
        } = prepared;
        let handoff_started = std::time::Instant::now();
        let handoff = async {
            // A host that died during startup must not hold the launch until the
            // handoff deadline; its exit releases the ordinary fallback at once.
            tokio::select! {
                connected = pipe.connect() => connected?,
                () = async {
                    while !process.session.has_exited() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => return Err(std::io::Error::other("speculative host exited before handoff")),
            }
            // Only the host itself may receive the script.
            if !pipe_client_is(&pipe, pid) {
                return Err(std::io::Error::other(
                    "handoff pipe client is not the speculative host",
                ));
            }
            let bytes = launch.script.as_bytes();
            let header = format!("{}\n{}\n{}\n", launch.thread_id, restore_class, bytes.len());
            pipe.write_all(header.as_bytes()).await?;
            pipe.write_all(bytes).await?;
            pipe.flush().await?;
            let mut ack = [0_u8; 1];
            pipe.read_exact(&mut ack).await?;
            Ok::<u8, std::io::Error>(ack[0])
        };
        let outcome = tokio::time::timeout(HANDOFF_TIMEOUT, handoff).await;
        let handoff_ms = handoff_started.elapsed().as_secs_f64() * 1000.0;
        let lead_ms = handoff_started.duration_since(started).as_secs_f64() * 1000.0;
        let reason = match outcome {
            Ok(Ok(b'A')) => {
                // The host now runs this call's command: record it as this call's
                // requested root, as an ordinary launch here would be.
                observation.adopt_into_current_scope();
                tracing::info!(target: "codex_speculative_shell", outcome = "accepted", handoff_ms, lead_ms, "speculative shell accepted");
                retire(pipe);
                return Some(process);
            }
            Ok(Ok(b'P')) => {
                LOGIN_PROFILE_DECLINED.store(true, Ordering::Release);
                "declined_profile".to_string()
            }
            Ok(Ok(_)) => "declined".to_string(),
            Ok(Err(error)) => format!("failed: {error}"),
            Err(_) => "timeout".to_string(),
        };
        tracing::info!(target: "codex_speculative_shell", outcome = %reason, handoff_ms, lead_ms, "speculative shell not used");
        retire((process, pipe));
        None
    }

    #[cfg(not(windows))]
    pub(crate) async fn take_and_hand_off(
        &self,
        _launch: &SpeculativeLaunch<'_>,
    ) -> Option<SpawnedProcess> {
        None
    }

    /// Tops up spares for the key just launched.
    pub(crate) fn replenish(&self, key: SpeculativeShellKey) {
        *self
            .last_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key.clone());
        #[cfg(windows)]
        tokio::spawn(top_up(key));
        #[cfg(not(windows))]
        let _ = key;
    }

    /// Lookahead: an announced shell-capable call re-arms this session's last key.
    pub(crate) fn prime(&self) {
        if !enabled() {
            return;
        }
        let Some(key) = self
            .last_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        #[cfg(windows)]
        tokio::spawn(top_up(key));
        #[cfg(not(windows))]
        let _ = key;
    }
}

/// Whether the connected client of `pipe` is process `pid`.
#[cfg(windows)]
fn pipe_client_is(pipe: &tokio::net::windows::named_pipe::NamedPipeServer, pid: u32) -> bool {
    use std::os::windows::io::AsRawHandle;
    let mut client = 0_u32;
    // SAFETY: the handle is the live server end owned by `pipe`; the out-pointer is
    // a valid u32 on this stack frame.
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(
            pipe.as_raw_handle(),
            &mut client,
        )
    };
    ok != 0 && pid != 0 && client == pid
}

#[cfg(windows)]
async fn spares_for(key: &SpeculativeShellKey) -> usize {
    POOL.lock()
        .await
        .iter()
        .filter(|prepared| prepared.key == *key)
        .count()
}

/// Starts spares one after another until the key has [`SPARES_PER_KEY`].
#[cfg(windows)]
async fn top_up(key: SpeculativeShellKey) {
    for _ in 0..SPARES_PER_KEY {
        if spares_for(&key).await >= SPARES_PER_KEY || !prepare(key.clone()).await {
            return;
        }
    }
}

/// Adds one spare for `key`; `false` when none was added.
#[cfg(windows)]
async fn prepare(key: SpeculativeShellKey) -> bool {
    use tokio::net::windows::named_pipe::ServerOptions;
    if key.is_login() && LOGIN_PROFILE_DECLINED.load(Ordering::Acquire) {
        return false;
    }
    if spares_for(&key).await >= SPARES_PER_KEY {
        return false;
    }
    let name = format!("kd4-speculative-shell-{}", uuid::Uuid::new_v4().simple());
    let pipe = match ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .create(format!(r"\\.\pipe\{name}"))
    {
        Ok(pipe) => pipe,
        Err(error) => {
            tracing::info!(target: "codex_speculative_shell", outcome = "pipe_failed", %error, "speculative shell not prepared");
            return false;
        }
    };
    let mut env = key
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    env.insert(BOOTSTRAP_PIPE_ENV.to_string(), name);
    // Never run profile code speculatively; the bootstrap verifies that a login
    // launch's profiles are empty before accepting.
    let mut args = Vec::with_capacity(key.flags.len() + 2);
    if key.is_login() {
        env.insert(BOOTSTRAP_LOGIN_ENV.to_string(), "1".to_string());
        args.push("-NoProfile".to_string());
    }
    args.extend(key.flags.iter().cloned());
    args.push(BOOTSTRAP.trim_end().to_string());
    let started = std::time::Instant::now();
    // The host runs below normal until handoff so speculative startup yields to
    // demand work; the bootstrap restores the class Windows assigned at creation
    // (parent, job and image-execution policy included) before running caller code.
    let (process, pid, observation) =
        match codex_utils_pty::pipe::spawn_process_no_stdin_lowered_until_handoff(
            &key.program,
            &args,
            &key.cwd,
            &env,
            &key.arg0,
        )
        .await
        {
            Ok(launched) => launched,
            Err(error) => {
                tracing::info!(target: "codex_speculative_shell", outcome = "spawn_failed", %error, "speculative shell not prepared");
                return false;
            }
        };
    let restore_class = process.session.deferred_priority_class().unwrap_or(0);
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let evicted = {
        let mut pool = POOL.lock().await;
        if pool.iter().filter(|prepared| prepared.key == key).count() >= SPARES_PER_KEY {
            // Concurrent launches already re-armed this key.
            drop(pool);
            retire((process, observation, pipe));
            return false;
        }
        let evicted = (pool.len() >= POOL_CAP).then(|| pool.remove(0));
        pool.push(Prepared {
            key,
            process,
            pid,
            observation,
            pipe,
            generation,
            started,
            restore_class,
        });
        evicted
    };
    if let Some(evicted) = evicted {
        tracing::info!(target: "codex_speculative_shell", outcome = "evicted", generation = evicted.generation, "speculative shell evicted");
        retire(evicted);
    }
    tracing::info!(target: "codex_speculative_shell", outcome = "prepared", generation, "speculative shell prepared");
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_LIMIT).await;
        let expired = {
            let mut pool = POOL.lock().await;
            pool.iter()
                .position(|prepared| prepared.generation == generation)
                .map(|index| pool.remove(index))
        };
        if let Some(expired) = expired {
            tracing::info!(target: "codex_speculative_shell", outcome = "expired", generation, "speculative shell expired");
            retire(expired);
        }
    });
    true
}

/// Response items that announce shell-capable work before their input completes.
pub(crate) fn announces_shell_work(item: &codex_protocol::models::ResponseItem) -> bool {
    use codex_protocol::models::ResponseItem;
    match item {
        ResponseItem::FunctionCall { name, .. } => name == "exec_command",
        ResponseItem::CustomToolCall { name, .. } => name == "exec",
        _ => false,
    }
}
