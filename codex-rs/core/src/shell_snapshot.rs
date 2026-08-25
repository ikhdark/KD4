use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use crate::StateDbHandle;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::rollout::list::find_thread_path_by_id_str;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::Environment;
use codex_exec_server::ExecEnvPolicy;
use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecParams;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::RemoveOptions;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tokio::fs;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::Instrument;
use tracing::info_span;

#[derive(Clone)]
pub(crate) struct ShellSnapshot {
    config: Option<Arc<ShellSnapshotConfig>>,
}

struct ShellSnapshotConfig {
    codex_home: AbsolutePathBuf,
    session_id: ThreadId,
    session_telemetry: SessionTelemetry,
    state_db: Option<StateDbHandle>,
    environment_variables: HashMap<String, String>,
    remote_environment_policy: ExecEnvPolicy,
}

pub(crate) struct ShellSnapshotFile {
    location: ShellSnapshotLocation,
    contents: String,
}

enum ShellSnapshotLocation {
    Local(AbsolutePathBuf),
    Remote {
        path: PathUri,
        filesystem: Arc<dyn ExecutorFileSystem>,
    },
}

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_RETENTION: Duration = Duration::from_secs(60 * 60 * 24);
const SNAPSHOT_DIR: &str = "shell_snapshots";
const EXCLUDED_EXPORT_VARS: &[&str] = &["PWD", "OLDPWD"];
pub(crate) const POWERSHELL_SNAPSHOT_FORMAT_HEADER: &str = "# Codex PowerShell snapshot format: 1";
pub(crate) const CMD_SNAPSHOT_FORMAT_HEADER: &str = "@rem Codex Cmd snapshot format: 1";
const REMOTE_SNAPSHOT_DIR: &str = ".codex-shell-snapshots";

fn exec_env_policy_from_shell_policy(policy: &ShellEnvironmentPolicy) -> ExecEnvPolicy {
    let mut exclude = policy
        .exclude
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    exclude.push(CODEX_PERMISSION_PROFILE_ENV_VAR.to_string());
    let mut r#set = policy.r#set.clone();
    r#set.retain(|key, _| !key.eq_ignore_ascii_case(CODEX_PERMISSION_PROFILE_ENV_VAR));
    ExecEnvPolicy {
        inherit: policy.inherit.clone(),
        ignore_default_excludes: policy.ignore_default_excludes,
        exclude,
        r#set,
        include_only: policy
            .include_only
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

impl ShellSnapshot {
    pub(crate) fn new(
        codex_home: AbsolutePathBuf,
        session_id: ThreadId,
        session_telemetry: SessionTelemetry,
        state_db: Option<StateDbHandle>,
        environment_variables: HashMap<String, String>,
        shell_environment_policy: ShellEnvironmentPolicy,
    ) -> Self {
        Self {
            config: Some(Arc::new(ShellSnapshotConfig {
                codex_home,
                session_id,
                session_telemetry,
                state_db,
                environment_variables,
                remote_environment_policy: exec_env_policy_from_shell_policy(
                    &shell_environment_policy,
                ),
            })),
        }
    }

    pub(crate) fn disabled() -> Self {
        Self { config: None }
    }

    pub(crate) async fn build(
        self,
        environment: TurnEnvironment,
    ) -> Option<Arc<ShellSnapshotFile>> {
        let config = self.config.as_ref()?;
        let shell = environment.shell.clone()?;
        if environment.environment.is_remote() {
            Self::build_for_remote_environment(
                Arc::clone(config),
                environment.cwd().clone(),
                shell,
                Arc::clone(&environment.environment),
            )
            .await
        } else {
            let cwd = environment.cwd().to_abs_path().ok()?;
            Self::build_for_cwd(Arc::clone(config), cwd, shell).await
        }
    }

    async fn build_for_remote_environment(
        config: Arc<ShellSnapshotConfig>,
        cwd: PathUri,
        shell: Shell,
        environment: Arc<Environment>,
    ) -> Option<Arc<ShellSnapshotFile>> {
        let snapshot_span = info_span!("shell_snapshot", thread_id = %config.session_id);
        async {
            let timer = config
                .session_telemetry
                .start_timer("codex.shell_snapshot.duration_ms", &[]);
            let snapshot =
                ShellSnapshot::try_create_remote(&config, &cwd, &shell, environment).await;
            let success_tag = if snapshot.is_ok() { "true" } else { "false" };
            let _ = timer.map(|timer| timer.record(&[("success", success_tag)]));
            let mut counter_tags = vec![("success", success_tag)];
            if let Some(failure_reason) = snapshot.as_ref().err() {
                counter_tags.push(("failure_reason", *failure_reason));
            }
            config
                .session_telemetry
                .counter("codex.shell_snapshot", /*inc*/ 1, &counter_tags);
            snapshot.ok().map(Arc::new)
        }
        .instrument(snapshot_span)
        .await
    }

    async fn build_for_cwd(
        config: Arc<ShellSnapshotConfig>,
        cwd: AbsolutePathBuf,
        shell: Shell,
    ) -> Option<Arc<ShellSnapshotFile>> {
        let snapshot_span = info_span!("shell_snapshot", thread_id = %config.session_id);
        async {
            let timer = config
                .session_telemetry
                .start_timer("codex.shell_snapshot.duration_ms", &[]);
            let snapshot = ShellSnapshot::try_create(
                &config.codex_home,
                config.session_id,
                &cwd,
                &shell,
                &config.environment_variables,
                config.state_db.clone(),
            )
            .await;
            let success_tag = if snapshot.is_ok() { "true" } else { "false" };
            let _ = timer.map(|timer| timer.record(&[("success", success_tag)]));
            let mut counter_tags = vec![("success", success_tag)];
            if let Some(failure_reason) = snapshot.as_ref().err() {
                counter_tags.push(("failure_reason", *failure_reason));
            }
            config
                .session_telemetry
                .counter("codex.shell_snapshot", /*inc*/ 1, &counter_tags);
            snapshot.ok().map(Arc::new)
        }
        .instrument(snapshot_span)
        .await
    }

    async fn try_create(
        codex_home: &AbsolutePathBuf,
        session_id: ThreadId,
        session_cwd: &AbsolutePathBuf,
        shell: &Shell,
        environment_variables: &HashMap<String, String>,
        state_db: Option<StateDbHandle>,
    ) -> std::result::Result<ShellSnapshotFile, &'static str> {
        // File to store the snapshot
        let extension = match shell.shell_type {
            ShellType::PowerShell => "ps1",
            ShellType::Cmd => "cmd",
            ShellType::Bash | ShellType::Zsh | ShellType::Sh => return Err("unsupported_shell"),
        };
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = codex_home
            .join(SNAPSHOT_DIR)
            .join(format!("{session_id}.{nonce}.{extension}"));
        let temp_path = codex_home
            .join(SNAPSHOT_DIR)
            .join(format!("{session_id}.tmp-{nonce}.{extension}"));

        // Clean the (unlikely) leaked snapshot files.
        let codex_home = codex_home.clone();
        let cleanup_session_id = session_id;
        tokio::spawn(async move {
            if let Err(err) =
                cleanup_stale_snapshots(&codex_home, cleanup_session_id, state_db).await
            {
                tracing::warn!("Failed to clean up shell snapshots: {err:?}");
            }
        });

        // Make the new snapshot.
        let contents =
            match write_shell_snapshot(shell, &temp_path, session_cwd, environment_variables).await
            {
                Ok(contents) => contents,
                Err(err) => {
                    tracing::warn!(
                        "Failed to create shell snapshot for {}: {err:?}",
                        shell.name()
                    );
                    return Err("write_failed");
                }
            };
        tracing::info!(
            "Shell snapshot successfully created: {}",
            temp_path.display()
        );

        if let Err(err) =
            validate_snapshot(shell, &temp_path, session_cwd, environment_variables).await
        {
            tracing::error!("Shell snapshot validation failed: {err:?}");
            remove_snapshot_file(&temp_path).await;
            return Err("validation_failed");
        }

        if let Err(err) = fs::rename(&temp_path, &path).await {
            tracing::warn!("Failed to finalize shell snapshot: {err:?}");
            remove_snapshot_file(&temp_path).await;
            return Err("write_failed");
        }

        Ok(ShellSnapshotFile {
            location: ShellSnapshotLocation::Local(path),
            contents,
        })
    }

    async fn try_create_remote(
        config: &ShellSnapshotConfig,
        session_cwd: &PathUri,
        shell: &Shell,
        environment: Arc<Environment>,
    ) -> std::result::Result<ShellSnapshotFile, &'static str> {
        let extension = match shell.shell_type {
            ShellType::PowerShell => "ps1",
            ShellType::Cmd => "cmd",
            ShellType::Bash | ShellType::Zsh | ShellType::Sh => return Err("unsupported_shell"),
        };
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let directory = session_cwd.join(REMOTE_SNAPSHOT_DIR).map_err(|err| {
            tracing::warn!("Failed to resolve remote shell snapshot directory: {err:?}");
            "write_failed"
        })?;
        let path = directory
            .join(&format!("{}.{nonce}.{extension}", config.session_id))
            .map_err(|err| {
                tracing::warn!("Failed to resolve remote shell snapshot path: {err:?}");
                "write_failed"
            })?;
        let filesystem = environment.get_filesystem();
        if let Err(err) = filesystem
            .create_directory(&directory, CreateDirectoryOptions { recursive: true }, None)
            .await
        {
            tracing::warn!("Failed to create remote shell snapshot directory: {err:?}");
            return Err("write_failed");
        }

        let mut remote_env = HashMap::new();
        if let Some(thread_id) = config.environment_variables.get("CODEX_THREAD_ID") {
            remote_env.insert("CODEX_THREAD_ID".to_string(), thread_id.clone());
        }
        let raw_snapshot = match capture_snapshot_remote(
            shell,
            session_cwd,
            &remote_env,
            config.remote_environment_policy.clone(),
            environment.as_ref(),
            config.session_id,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::warn!("Failed to capture remote shell snapshot: {err:?}");
                return Err("write_failed");
            }
        };
        let contents = match format_snapshot(shell.shell_type, &raw_snapshot) {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!("Failed to format remote shell snapshot: {err:?}");
                return Err("write_failed");
            }
        };
        if let Err(err) = filesystem
            .write_file(&path, contents.as_bytes().to_vec(), None)
            .await
        {
            tracing::warn!("Failed to write remote shell snapshot: {err:?}");
            return Err("write_failed");
        }
        if let Err(err) = validate_snapshot_remote(
            shell,
            &path,
            session_cwd,
            &remote_env,
            config.remote_environment_policy.clone(),
            environment.as_ref(),
            config.session_id,
        )
        .await
        {
            tracing::warn!("Remote shell snapshot validation failed: {err:?}");
            let _ = filesystem
                .remove(
                    &path,
                    RemoveOptions {
                        recursive: false,
                        force: true,
                    },
                    None,
                )
                .await;
            return Err("validation_failed");
        }

        Ok(ShellSnapshotFile {
            location: ShellSnapshotLocation::Remote { path, filesystem },
            contents,
        })
    }
}

impl ShellSnapshotFile {
    #[cfg(test)]
    pub(crate) fn path(&self) -> AbsolutePathBuf {
        match &self.location {
            ShellSnapshotLocation::Local(path) => path.clone(),
            ShellSnapshotLocation::Remote { .. } => {
                panic!("remote shell snapshot does not have a local path")
            }
        }
    }

    pub(crate) fn native_path_string(&self) -> String {
        match &self.location {
            ShellSnapshotLocation::Local(path) => path.to_string_lossy().into_owned(),
            ShellSnapshotLocation::Remote { path, .. } => path.inferred_native_path_string(),
        }
    }

    pub(crate) fn contents(&self) -> &str {
        &self.contents
    }
}

impl Drop for ShellSnapshotFile {
    fn drop(&mut self) {
        match &self.location {
            ShellSnapshotLocation::Local(path) => {
                if let Err(err) = std::fs::remove_file(path) {
                    tracing::warn!("Failed to delete shell snapshot at {:?}: {err:?}", path);
                }
            }
            ShellSnapshotLocation::Remote { path, filesystem } => {
                let path = path.clone();
                let filesystem = Arc::clone(filesystem);
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        if let Err(err) = filesystem
                            .remove(
                                &path,
                                RemoveOptions {
                                    recursive: false,
                                    force: true,
                                },
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                "Failed to delete remote shell snapshot at {path}: {err:?}"
                            );
                        }
                    });
                }
            }
        }
    }
}

async fn write_shell_snapshot(
    shell: &Shell,
    output_path: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    let raw_snapshot = capture_snapshot(shell, cwd, environment_variables).await?;
    let snapshot = format_snapshot(shell.shell_type, &raw_snapshot)?;

    if let Some(parent) = output_path.parent() {
        let parent_display = parent.display();
        fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("Failed to create snapshot parent {parent_display}"))?;
    }

    let snapshot_path = output_path.display();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    let mut file = options
        .open(output_path)
        .await
        .with_context(|| format!("Failed to create snapshot at {snapshot_path}"))?;
    file.write_all(snapshot.as_bytes())
        .await
        .with_context(|| format!("Failed to write snapshot to {snapshot_path}"))?;
    file.sync_all()
        .await
        .with_context(|| format!("Failed to persist snapshot to {snapshot_path}"))?;

    Ok(snapshot)
}

fn format_snapshot(shell_type: ShellType, raw_snapshot: &str) -> Result<String> {
    let snapshot = strip_snapshot_preamble(raw_snapshot)?;
    let format_header = match shell_type {
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {
            bail!("non-Windows shell snapshots are unsupported")
        }
        ShellType::PowerShell => POWERSHELL_SNAPSHOT_FORMAT_HEADER,
        ShellType::Cmd => "# Codex Cmd snapshot format: 1",
    };
    if !snapshot.lines().any(|line| line == format_header) {
        bail!("Snapshot output missing format marker {format_header}");
    }
    if shell_type == ShellType::Cmd {
        return format_cmd_snapshot(&snapshot);
    }
    Ok(snapshot)
}

fn format_cmd_snapshot(snapshot: &str) -> Result<String> {
    let exports = snapshot
        .lines()
        .skip_while(|line| *line != "# exports")
        .skip(1);
    let mut formatted =
        format!("@rem Snapshot file\r\n{CMD_SNAPSHOT_FORMAT_HEADER}\r\n@rem exports\r\n");
    for line in exports {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if !is_valid_cmd_environment_name(name)
            || EXCLUDED_EXPORT_VARS
                .iter()
                .any(|excluded| name.eq_ignore_ascii_case(excluded))
            || name
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("__CODEX_"))
        {
            continue;
        }
        let escaped_value = escape_cmd_set_value(value);
        formatted.push_str(&format!("@set {name}={escaped_value}\r\n"));
    }
    Ok(formatted)
}

fn escape_cmd_set_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '^' | '&' | '|' | '<' | '>' | '(' | ')' | '"' => {
                escaped.push('^');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

pub(crate) fn parse_cmd_snapshot_environment(snapshot: &str) -> Option<Vec<(String, String)>> {
    if !snapshot
        .lines()
        .any(|line| line == CMD_SNAPSHOT_FORMAT_HEADER)
    {
        return None;
    }
    Some(
        snapshot
            .lines()
            .filter_map(|line| line.strip_prefix("@set "))
            .filter_map(|assignment| assignment.split_once('='))
            .filter(|(name, _)| is_valid_cmd_environment_name(name))
            .map(|(name, value)| (name.to_string(), unescape_cmd_set_value(value)))
            .collect(),
    )
}

fn unescape_cmd_set_value(value: &str) -> String {
    let mut unescaped = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match (character, characters.peek().copied()) {
            ('%', Some('%')) => {
                characters.next();
                unescaped.push('%');
            }
            ('^', Some(escaped)) => {
                characters.next();
                unescaped.push(escaped);
            }
            _ => unescaped.push(character),
        }
    }
    unescaped
}

fn is_valid_cmd_environment_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|character| {
            matches!(
                character,
                '=' | '"'
                    | '%'
                    | '!'
                    | '^'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '\r'
                    | '\n'
                    | '\0'
            )
        })
}

async fn capture_snapshot(
    shell: &Shell,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    let shell_type = shell.shell_type;
    match shell_type {
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {
            bail!("non-Windows shell snapshots are unsupported")
        }
        ShellType::PowerShell => {
            run_shell_script(
                shell,
                powershell_snapshot_script(),
                cwd,
                environment_variables,
            )
            .await
        }
        ShellType::Cmd => {
            run_shell_script(
                shell,
                "@echo # Snapshot file&@echo # Codex Cmd snapshot format: 1&@echo # exports&@set",
                cwd,
                environment_variables,
            )
            .await
        }
    }
}

async fn capture_snapshot_remote(
    shell: &Shell,
    cwd: &PathUri,
    environment_variables: &HashMap<String, String>,
    environment_policy: ExecEnvPolicy,
    environment: &Environment,
    session_id: ThreadId,
) -> Result<String> {
    let script = match shell.shell_type {
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {
            bail!("non-Windows shell snapshots are unsupported")
        }
        ShellType::PowerShell => powershell_snapshot_script().to_string(),
        ShellType::Cmd => {
            "@echo # Snapshot file&@echo # Codex Cmd snapshot format: 1&@echo # exports&@set"
                .to_string()
        }
    };
    run_remote_script_with_timeout(
        shell,
        &script,
        SNAPSHOT_TIMEOUT,
        /*use_login_shell*/ true,
        cwd,
        environment_variables,
        environment_policy,
        environment,
        session_id,
    )
    .await
}

fn strip_snapshot_preamble(snapshot: &str) -> Result<String> {
    let marker = "# Snapshot file";
    let Some(start) = snapshot.find(marker) else {
        bail!("Snapshot output missing marker {marker}");
    };

    Ok(snapshot[start..].to_string())
}

fn powershell_single_quote(input: &str) -> String {
    input.replace('\'', "''")
}

async fn validate_snapshot(
    shell: &Shell,
    snapshot_path: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<()> {
    match shell.shell_type {
        ShellType::PowerShell => {
            let snapshot_path = powershell_single_quote(&snapshot_path.to_string_lossy());
            let script = format!("$ErrorActionPreference = 'Stop'; . '{snapshot_path}'");
            run_script_with_timeout(
                shell,
                &script,
                SNAPSHOT_TIMEOUT,
                /*use_login_shell*/ false,
                cwd,
                environment_variables,
            )
            .await
            .map(|_| ())
        }
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {
            bail!("non-Windows shell snapshots are unsupported")
        }
        ShellType::Cmd => {
            let snapshot = fs::read_to_string(snapshot_path).await?;
            parse_cmd_snapshot_environment(&snapshot)
                .context("Cmd snapshot is not parseable")
                .map(|_| ())
        }
    }
}

async fn validate_snapshot_remote(
    shell: &Shell,
    snapshot_path: &PathUri,
    cwd: &PathUri,
    environment_variables: &HashMap<String, String>,
    environment_policy: ExecEnvPolicy,
    environment: &Environment,
    session_id: ThreadId,
) -> Result<()> {
    let mut environment_variables = environment_variables.clone();
    environment_variables.insert(
        "__CODEX_SNAPSHOT_FILE".to_string(),
        snapshot_path.inferred_native_path_string(),
    );
    let script = match shell.shell_type {
        ShellType::PowerShell => "$ErrorActionPreference = 'Stop'; . $env:__CODEX_SNAPSHOT_FILE",
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {
            bail!("non-Windows shell snapshots are unsupported")
        }
        ShellType::Cmd => "@call \"%__CODEX_SNAPSHOT_FILE%\"",
    };
    run_remote_script_with_timeout(
        shell,
        script,
        SNAPSHOT_TIMEOUT,
        /*use_login_shell*/ false,
        cwd,
        &environment_variables,
        environment_policy,
        environment,
        session_id,
    )
    .await
    .map(|_| ())
}

async fn run_shell_script(
    shell: &Shell,
    script: &str,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    run_script_with_timeout(
        shell,
        script,
        SNAPSHOT_TIMEOUT,
        /*use_login_shell*/ true,
        cwd,
        environment_variables,
    )
    .await
}

async fn run_script_with_timeout(
    shell: &Shell,
    script: &str,
    snapshot_timeout: Duration,
    use_login_shell: bool,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    run_script_with_timeout_with_args(
        shell,
        script,
        &[],
        snapshot_timeout,
        use_login_shell,
        cwd,
        environment_variables,
    )
    .await
}

async fn run_script_with_timeout_with_args(
    shell: &Shell,
    script: &str,
    script_args: &[&OsStr],
    snapshot_timeout: Duration,
    use_login_shell: bool,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    let args = shell.derive_exec_args(script, use_login_shell);
    let args = clean_snapshot_shell_args(shell.shell_type, args, use_login_shell);
    let shell_name = shell.name();

    // Handler is kept as guard to control the drop. The `mut` pattern is required because .args()
    // returns a ref of handler.
    let mut handler = Command::new(&args[0]);
    handler.args(&args[1..]);
    handler.args(script_args);
    handler.stdin(Stdio::null());
    handler.current_dir(cwd);
    handler.env_clear();
    handler.envs(environment_variables);

    handler.kill_on_drop(true);
    handler.stdout(Stdio::piped());
    handler.stderr(Stdio::piped());

    let mut child = handler
        .spawn()
        .with_context(|| format!("Failed to execute {shell_name}"))?;
    let process_group_id = child.id();
    let mut stdout = child
        .stdout
        .take()
        .context("Snapshot command stdout was not piped")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Snapshot command stderr was not piped")?;

    let output = timeout(snapshot_timeout, async {
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let (status, stdout_read, stderr_read) = tokio::join!(
            child.wait(),
            read_snapshot_output_bounded(&mut stdout, &mut stdout_bytes),
            read_snapshot_output_bounded(&mut stderr, &mut stderr_bytes),
        );
        let status = status.with_context(|| format!("Failed to execute {shell_name}"))?;
        let stdout_overflow = stdout_read.context("Failed to read snapshot command stdout")?;
        let stderr_overflow = stderr_read.context("Failed to read snapshot command stderr")?;
        if stdout_overflow || stderr_overflow {
            bail!(
                "Snapshot command output exceeded the {SNAPSHOT_OUTPUT_LIMIT_BYTES} byte per-stream limit"
            );
        }
        Ok::<_, anyhow::Error>((status, stdout_bytes, stderr_bytes))
    })
    .await;

    let (status, stdout, stderr) = match output {
        Ok(output) => output?,
        Err(_) => {
            if let Some(process_group_id) = process_group_id
                && let Err(err) =
                    codex_utils_pty::process_group::kill_process_group(process_group_id)
            {
                tracing::warn!(
                    "Failed to kill timed-out snapshot process group {process_group_id}: {err:?}"
                );
            }
            if let Err(err) = child.start_kill()
                && err.kind() != ErrorKind::InvalidInput
                && err.kind() != ErrorKind::NotFound
            {
                tracing::warn!("Failed to kill timed-out snapshot shell: {err:?}");
            }
            drop(stdout);
            drop(stderr);
            match timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Failed to reap timed-out snapshot shell: {err:?}");
                }
                Err(_) => {
                    tracing::warn!("Timed out reaping killed snapshot shell");
                }
            }
            return Err(anyhow!("Snapshot command timed out for {shell_name}"));
        }
    };

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        bail!("Snapshot command exited with status {status}: {stderr}");
    }

    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn clean_snapshot_shell_args(
    shell_type: ShellType,
    args: Vec<String>,
    use_login_shell: bool,
) -> Vec<String> {
    if shell_type != ShellType::Cmd || args.is_empty() {
        return args;
    }
    let mut cleaned = Vec::with_capacity(args.len() + 2);
    cleaned.push(args[0].clone());
    if !use_login_shell {
        cleaned.push("/d".to_string());
    }
    cleaned.push("/v:off".to_string());
    cleaned.extend(args[1..].iter().cloned());
    cleaned
}

#[allow(clippy::too_many_arguments)]
async fn run_remote_script_with_timeout(
    shell: &Shell,
    script: &str,
    snapshot_timeout: Duration,
    use_login_shell: bool,
    cwd: &PathUri,
    environment_variables: &HashMap<String, String>,
    environment_policy: ExecEnvPolicy,
    environment: &Environment,
    session_id: ThreadId,
) -> Result<String> {
    let args = clean_snapshot_shell_args(
        shell.shell_type,
        shell.derive_exec_args(script, use_login_shell),
        use_login_shell,
    );
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let started = timeout(
        snapshot_timeout,
        environment.get_exec_backend().start(ExecParams {
            process_id: format!("{session_id}-shell-snapshot-{nonce}").into(),
            argv: args,
            cwd: cwd.clone(),
            env_policy: Some(environment_policy),
            env: environment_variables.clone(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        }),
    )
    .await
    .map_err(|_| anyhow!("Snapshot command timed out for {}", shell.name()))??;
    let process = started.process;
    let collect = async {
        let mut after_seq = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        loop {
            let response = process
                .read(
                    after_seq,
                    Some(SNAPSHOT_OUTPUT_LIMIT_BYTES.saturating_add(1)),
                    Some(1_000),
                )
                .await
                .context("Failed to read remote snapshot command output")?;
            if let Some(failure) = response.failure {
                bail!("Remote snapshot command failed: {failure}");
            }
            for chunk in response.chunks {
                after_seq = Some(chunk.seq);
                let bytes = chunk.chunk.into_inner();
                let retained = match chunk.stream {
                    ExecOutputStream::Stdout | ExecOutputStream::Pty => &mut stdout,
                    ExecOutputStream::Stderr => &mut stderr,
                };
                if retained.len().saturating_add(bytes.len()) > SNAPSHOT_OUTPUT_LIMIT_BYTES {
                    bail!(
                        "Snapshot command output exceeded the {SNAPSHOT_OUTPUT_LIMIT_BYTES} byte per-stream limit"
                    );
                }
                retained.extend_from_slice(&bytes);
            }
            after_seq = response.next_seq.checked_sub(1).or(after_seq);
            exit_code = response.exit_code.or(exit_code);
            if response.closed {
                break;
            }
        }
        let exit_code = exit_code.unwrap_or(-1);
        if exit_code != 0 {
            bail!(
                "Snapshot command exited with status {exit_code}: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&stdout).into_owned())
    };
    match timeout(snapshot_timeout, collect).await {
        Ok(output) => output,
        Err(_) => {
            if let Err(err) = process.terminate().await {
                tracing::warn!("Failed to terminate timed-out remote snapshot shell: {err:?}");
            }
            Err(anyhow!("Snapshot command timed out for {}", shell.name()))
        }
    }
}

const SNAPSHOT_OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Retains at most the configured limit while continuing to drain the pipe so
/// a verbose child cannot deadlock waiting for its reader.
async fn read_snapshot_output_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
    retained: &mut Vec<u8>,
) -> std::io::Result<bool> {
    let mut overflow = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(overflow);
        }
        let remaining = SNAPSHOT_OUTPUT_LIMIT_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        overflow |= keep < read;
    }
}

fn powershell_snapshot_script() -> &'static str {
    r##"$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}
Microsoft.PowerShell.Utility\Write-Output '# Snapshot file'
Microsoft.PowerShell.Utility\Write-Output '# Codex PowerShell snapshot format: 1'
Microsoft.PowerShell.Utility\Write-Output '# Unset all aliases to avoid conflicts with functions'
Microsoft.PowerShell.Utility\Write-Output 'Microsoft.PowerShell.Management\Remove-Item -Path Alias:* -Force -ErrorAction SilentlyContinue'
Microsoft.PowerShell.Utility\Write-Output '# Functions'
Microsoft.PowerShell.Management\Get-ChildItem Function: | Microsoft.PowerShell.Core\ForEach-Object {
    $encodedName = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Name)
    )
    $encodedDefinition = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Definition)
    )
    Microsoft.PowerShell.Utility\Write-Output (
        "Microsoft.PowerShell.Management\Set-Item -LiteralPath ('Function:' + [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{0}'))) -Value ([System.Management.Automation.ScriptBlock]::Create([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{1}'))))" -f $encodedName, $encodedDefinition
    )
}
Microsoft.PowerShell.Utility\Write-Output ''
$aliases = Microsoft.PowerShell.Utility\Get-Alias
Microsoft.PowerShell.Utility\Write-Output '# aliases'
$aliases | Microsoft.PowerShell.Core\ForEach-Object {
    $encodedName = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Name)
    )
    $encodedDefinition = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Definition)
    )
    Microsoft.PowerShell.Utility\Write-Output (
        "Microsoft.PowerShell.Utility\Set-Alias -Name ([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{0}'))) -Value ([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{1}')))" -f $encodedName, $encodedDefinition
    )
}
Microsoft.PowerShell.Utility\Write-Output ''
$envVars = Microsoft.PowerShell.Management\Get-ChildItem Env:
Microsoft.PowerShell.Utility\Write-Output '# exports'
$envVars | Microsoft.PowerShell.Core\ForEach-Object {
    if ($_.Name -in @('PWD', 'OLDPWD') -or $_.Name -like '__CODEX_SNAPSHOT_*') {
        return
    }
    $encodedName = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Name)
    )
    $encodedValue = [System.Convert]::ToBase64String(
        [System.Text.Encoding]::UTF8.GetBytes([string]$_.Value)
    )
    Microsoft.PowerShell.Utility\Write-Output (
        "Microsoft.PowerShell.Management\Set-Item -LiteralPath ('Env:' + [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{0}'))) -Value ([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{1}')))" -f $encodedName, $encodedValue
    )
}
"##
}

/// Removes shell snapshots that either lack a matching session rollout file or
/// whose rollouts have not been updated within the retention window.
/// The active session id is exempt from cleanup.
pub async fn cleanup_stale_snapshots(
    codex_home: &AbsolutePathBuf,
    active_session_id: ThreadId,
    state_db: Option<StateDbHandle>,
) -> Result<()> {
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);

    let mut entries = match fs::read_dir(&snapshot_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let now = SystemTime::now();
    let active_session_id = active_session_id.to_string();

    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }

        let path = entry.path();

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(session_id) = snapshot_session_id_from_file_name(&file_name) else {
            remove_snapshot_file(&path).await;
            continue;
        };
        if session_id == active_session_id {
            continue;
        }

        let rollout_path =
            find_thread_path_by_id_str(codex_home, session_id, state_db.as_deref()).await?;
        let Some(rollout_path) = rollout_path else {
            remove_snapshot_file(&path).await;
            continue;
        };

        let modified = match fs::metadata(&rollout_path).await.and_then(|m| m.modified()) {
            Ok(modified) => modified,
            Err(err) => {
                tracing::warn!(
                    "Failed to check rollout age for snapshot {}: {err:?}",
                    path.display()
                );
                continue;
            }
        };

        if now
            .duration_since(modified)
            .ok()
            .is_some_and(|age| age >= SNAPSHOT_RETENTION)
        {
            remove_snapshot_file(&path).await;
        }
    }

    Ok(())
}

async fn remove_snapshot_file(path: &Path) {
    if let Err(err) = fs::remove_file(path).await {
        tracing::warn!("Failed to delete shell snapshot at {:?}: {err:?}", path);
    }
}

fn snapshot_session_id_from_file_name(file_name: &str) -> Option<&str> {
    let (stem, extension) = file_name.rsplit_once('.')?;
    match extension {
        "ps1" | "cmd" => Some(
            stem.split_once('.')
                .map_or(stem, |(session_id, _generation)| session_id),
        ),
        _ if extension.starts_with("tmp-") => Some(stem),
        _ => None,
    }
}

#[cfg(test)]
#[path = "shell_snapshot_tests.rs"]
mod tests;
