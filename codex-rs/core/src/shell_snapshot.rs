use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use crate::StateDbHandle;
use crate::rollout::list::find_thread_path_by_id_str;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs;
use tokio::io::AsyncReadExt;
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
}

pub(crate) struct ShellSnapshotFile {
    path: AbsolutePathBuf,
}

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_RETENTION: Duration = Duration::from_secs(60 * 60 * 24 * 3); // 3 days retention.
const SNAPSHOT_DIR: &str = "shell_snapshots";
const EXCLUDED_EXPORT_VARS: &[&str] = &["PWD", "OLDPWD"];
pub(crate) const POSIX_SNAPSHOT_FORMAT_HEADER: &str = "# Codex shell snapshot format: 3";

impl ShellSnapshot {
    pub(crate) fn new(
        codex_home: AbsolutePathBuf,
        session_id: ThreadId,
        session_telemetry: SessionTelemetry,
        state_db: Option<StateDbHandle>,
        environment_variables: HashMap<String, String>,
    ) -> Self {
        Self {
            config: Some(Arc::new(ShellSnapshotConfig {
                codex_home,
                session_id,
                session_telemetry,
                state_db,
                environment_variables,
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
        if environment.environment.is_remote() {
            return None;
        }

        let shell = environment.shell.clone()?;
        // TODO(anp): Migrate shell snapshot creation to accept PathUri and defer native
        // conversion to the spawned shell process.
        let cwd = environment.cwd().to_abs_path().ok()?;
        Self::build_for_cwd(Arc::clone(config), cwd, shell).await
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
            _ => "sh",
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
            .join(format!("{session_id}.tmp-{nonce}"));

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
        if let Err(err) =
            write_shell_snapshot(shell, &temp_path, session_cwd, environment_variables).await
        {
            tracing::warn!(
                "Failed to create shell snapshot for {}: {err:?}",
                shell.name()
            );
            return Err("write_failed");
        }
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

        Ok(ShellSnapshotFile { path })
    }
}

impl ShellSnapshotFile {
    pub(crate) fn path(&self) -> AbsolutePathBuf {
        self.path.clone()
    }
}

impl Drop for ShellSnapshotFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                "Failed to delete shell snapshot at {:?}: {err:?}",
                self.path
            );
        }
    }
}

async fn write_shell_snapshot(
    shell: &Shell,
    output_path: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<()> {
    let shell_type = shell.shell_type;
    if shell_type == ShellType::PowerShell || shell_type == ShellType::Cmd {
        bail!("Shell snapshot not supported yet for {shell_type:?}");
    }

    let raw_snapshot = capture_snapshot(shell, cwd, environment_variables).await?;
    let snapshot = strip_snapshot_preamble(&raw_snapshot)?;
    if matches!(shell_type, ShellType::Bash | ShellType::Zsh | ShellType::Sh)
        && !snapshot
            .lines()
            .any(|line| line == POSIX_SNAPSHOT_FORMAT_HEADER)
    {
        bail!("Snapshot output missing format marker {POSIX_SNAPSHOT_FORMAT_HEADER}");
    }

    if let Some(parent) = output_path.parent() {
        let parent_display = parent.display();
        fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("Failed to create snapshot parent {parent_display}"))?;
    }

    let snapshot_path = output_path.display();
    fs::write(output_path, snapshot)
        .await
        .with_context(|| format!("Failed to write snapshot to {snapshot_path}"))?;

    Ok(())
}

async fn capture_snapshot(
    shell: &Shell,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<String> {
    let shell_type = shell.shell_type;
    match shell_type {
        ShellType::Zsh => {
            run_shell_script(shell, &zsh_snapshot_script(), cwd, environment_variables).await
        }
        ShellType::Bash => {
            run_shell_script(shell, &bash_snapshot_script(), cwd, environment_variables).await
        }
        ShellType::Sh => {
            run_shell_script(shell, &sh_snapshot_script(), cwd, environment_variables).await
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
        ShellType::Cmd => bail!("Shell snapshotting is not yet supported for {shell_type:?}"),
    }
}

fn strip_snapshot_preamble(snapshot: &str) -> Result<String> {
    let marker = "# Snapshot file";
    let Some(start) = snapshot.find(marker) else {
        bail!("Snapshot output missing marker {marker}");
    };

    Ok(snapshot[start..].to_string())
}

async fn validate_snapshot(
    shell: &Shell,
    snapshot_path: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    environment_variables: &HashMap<String, String>,
) -> Result<()> {
    let positional_args = [
        OsStr::new("codex-shell-snapshot-validation"),
        snapshot_path.as_os_str(),
    ];
    run_script_with_timeout_with_args(
        shell,
        r#"\command set -e; \command . "$1""#,
        &positional_args,
        SNAPSHOT_TIMEOUT,
        /*use_login_shell*/ false,
        cwd,
        environment_variables,
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
    #[cfg(unix)]
    unsafe {
        handler.pre_exec(|| {
            codex_utils_pty::process_group::detach_from_tty()?;
            Ok(())
        });
    }
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
            stdout.read_to_end(&mut stdout_bytes),
            stderr.read_to_end(&mut stderr_bytes),
        );
        let status = status.with_context(|| format!("Failed to execute {shell_name}"))?;
        stdout_read.context("Failed to read snapshot command stdout")?;
        stderr_read.context("Failed to read snapshot command stderr")?;
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
            if let Err(err) = child.wait().await {
                tracing::warn!("Failed to reap timed-out snapshot shell: {err:?}");
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

fn excluded_exports_regex() -> String {
    EXCLUDED_EXPORT_VARS.join("|")
}

fn zsh_snapshot_script() -> String {
    let excluded = excluded_exports_regex();
    let script = r##"if [[ "${(t)functions}" == association* && -z "${functions[builtin]-}" && -z "${functions[command]-}" ]]; then
  __CODEX_SNAPSHOT_RC="${ZDOTDIR:-$HOME}/.zshrc"
  \builtin test ! -r "$__CODEX_SNAPSHOT_RC" || \builtin source "$__CODEX_SNAPSHOT_RC"
  if [[ "${(t)functions}" == association* && -z "${functions[builtin]-}" && -z "${functions[command]-}" ]]; then
    __CODEX_SNAPSHOT_FUNCTIONS=$(\builtin functions)
    __CODEX_SNAPSHOT_ALIAS_LINES=$(\builtin alias -L 2>/dev/null) || __CODEX_SNAPSHOT_ALIAS_LINES=
    \builtin unalias -a 2>/dev/null || \builtin true
    \builtin print '# Snapshot file'
    \builtin print '# Codex shell snapshot format: 3'
    \builtin print '# Unset all aliases to avoid conflicts with functions'
    \builtin print -r -- '\builtin unalias -a 2>/dev/null || \builtin true'
    \builtin print '# setopts'
    while IFS= \builtin read -r __CODEX_SNAPSHOT_ZSH_OPT; do
      \builtin print -r -- "\\builtin setopt $__CODEX_SNAPSHOT_ZSH_OPT"
    done < <(\builtin setopt)
    \builtin print ''
    \builtin print '# Functions'
    __CODEX_SNAPSHOT_FUNCTIONS_ESCAPED=$(\builtin print -rn -- "$__CODEX_SNAPSHOT_FUNCTIONS" | \command sed "s/'/'\"'\"'/g")
    \builtin print -r -- "__CODEX_SNAPSHOT_FUNCTIONS='$__CODEX_SNAPSHOT_FUNCTIONS_ESCAPED'"
    \builtin print ''
    \builtin print '# aliases'
    __CODEX_SNAPSHOT_ALIASES=$(
      if [[ -n "$__CODEX_SNAPSHOT_ALIAS_LINES" ]]; then
        while IFS= \builtin read -r __CODEX_SNAPSHOT_ALIAS_LINE; do
          \builtin print -r -- "\\builtin $__CODEX_SNAPSHOT_ALIAS_LINE"
        done <<< "$__CODEX_SNAPSHOT_ALIAS_LINES"
      fi
    )
    __CODEX_SNAPSHOT_ALIASES_ESCAPED=$(\builtin print -rn -- "$__CODEX_SNAPSHOT_ALIASES" | \command sed "s/'/'\"'\"'/g")
    \builtin print -r -- "__CODEX_SNAPSHOT_ALIASES='$__CODEX_SNAPSHOT_ALIASES_ESCAPED'"
    \builtin print ''
    __CODEX_SNAPSHOT_EXPORT_LINES=$(\builtin export -p | \command awk '
/^(export|declare -x|typeset -x) / {
  line=$0
  name=line
  sub(/^(export|declare -x|typeset -x) /, "", name)
  sub(/=.*/, "", name)
  if (name ~ /^__CODEX_SNAPSHOT_/ || name ~ /^(EXCLUDED_EXPORTS)$/) {
    next
  }
  if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) {
    print line
  }
}')
    \builtin print '# exports'
    if [[ -n "$__CODEX_SNAPSHOT_EXPORT_LINES" ]]; then
      while IFS= \builtin read -r __CODEX_SNAPSHOT_EXPORT_LINE; do
        \builtin print -r -- "\\builtin $__CODEX_SNAPSHOT_EXPORT_LINE"
      done <<< "$__CODEX_SNAPSHOT_EXPORT_LINES"
    fi
    \builtin true
  else
    [[ 1 == 0 ]]
  fi
else
  [[ 1 == 0 ]]
fi
"##;
    script.replace("EXCLUDED_EXPORTS", &excluded)
}

fn bash_snapshot_script() -> String {
    let excluded = excluded_exports_regex();
    let script = r##"if [[ -o posix ]]; then
  __CODEX_SNAPSHOT_POSIX_WAS_SET=1
else
  __CODEX_SNAPSHOT_POSIX_WAS_SET=0
fi
\set -o posix
if [[ -o posix ]] && ! \readonly -f builtin 2>/dev/null && ! \readonly -f command 2>/dev/null; then
  if [[ "$__CODEX_SNAPSHOT_POSIX_WAS_SET" != 1 ]]; then
    \set +o posix
  fi
  \builtin test -n "$BASH_ENV" || \builtin test ! -r "$HOME/.bashrc" || \builtin source "$HOME/.bashrc"
  if [[ -o posix ]]; then
    __CODEX_SNAPSHOT_POSIX_WAS_SET=1
  else
    __CODEX_SNAPSHOT_POSIX_WAS_SET=0
  fi
  \set -o posix
  if [[ -o posix ]] && ! \readonly -f builtin 2>/dev/null && ! \readonly -f command 2>/dev/null; then
    if [[ "$__CODEX_SNAPSHOT_POSIX_WAS_SET" != 1 ]]; then
      \set +o posix
    fi
    if \builtin declare -xp BASH_ENV >/dev/null 2>&1; then
      __CODEX_SNAPSHOT_BASH_ENV_PRESENT=1
    else
      __CODEX_SNAPSHOT_BASH_ENV_PRESENT=0
    fi
    __CODEX_SNAPSHOT_FUNCTIONS=$(\builtin declare -f)
    __CODEX_SNAPSHOT_ALIAS_LINES=$(\builtin alias -p 2>/dev/null) || __CODEX_SNAPSHOT_ALIAS_LINES=
    \builtin unalias -a 2>/dev/null || \builtin true
    \builtin printf '%s\n' '# Snapshot file'
    \builtin printf '%s\n' '# Codex shell snapshot format: 3'
    \builtin printf '__CODEX_SNAPSHOT_BASH_ENV_PRESENT=%s\n' "$__CODEX_SNAPSHOT_BASH_ENV_PRESENT"
    \builtin printf '%s\n' '# Unset all aliases to avoid conflicts with functions'
    \builtin printf '%s\n' '\builtin unalias -a 2>/dev/null || \builtin true'
    \builtin printf '%s\n' '# shopts'
    while IFS= \builtin read -r __CODEX_SNAPSHOT_SHOPT_LINE; do
      \builtin printf '%s %s\n' '\builtin' "$__CODEX_SNAPSHOT_SHOPT_LINE"
    done < <(\builtin shopt -p)
    \builtin printf '\n'
    __CODEX_SNAPSHOT_BASH_OPTS=$(\builtin set -o | \command awk '$2=="on"{print $1}')
    \builtin printf '%s\n' '# setopts'
    if [[ -n "$__CODEX_SNAPSHOT_BASH_OPTS" ]]; then
      for __CODEX_SNAPSHOT_BASH_OPT in $__CODEX_SNAPSHOT_BASH_OPTS; do
        \builtin printf '%s set -o %s\n' '\builtin' "$__CODEX_SNAPSHOT_BASH_OPT"
      done
    fi
    \builtin printf '\n'
    \builtin printf '%s\n' '# Functions'
    \builtin printf '__CODEX_SNAPSHOT_FUNCTIONS=%q\n' "$__CODEX_SNAPSHOT_FUNCTIONS"
    \builtin printf '\n'
    \builtin printf '%s\n' '# aliases'
    __CODEX_SNAPSHOT_ALIASES=$(
      if [[ -n "$__CODEX_SNAPSHOT_ALIAS_LINES" ]]; then
        while IFS= \builtin read -r __CODEX_SNAPSHOT_ALIAS_LINE; do
          \builtin printf '%s %s\n' '\builtin' "$__CODEX_SNAPSHOT_ALIAS_LINE"
        done <<< "$__CODEX_SNAPSHOT_ALIAS_LINES"
      fi
    )
    \builtin printf '__CODEX_SNAPSHOT_ALIASES=%q\n' "$__CODEX_SNAPSHOT_ALIASES"
    \builtin printf '\n'
    \builtin printf '%s\n' '# exports'
    while IFS= \builtin read -r __CODEX_SNAPSHOT_NAME; do
      if [[ "$__CODEX_SNAPSHOT_NAME" == __CODEX_SNAPSHOT_* || "$__CODEX_SNAPSHOT_NAME" =~ ^(EXCLUDED_EXPORTS)$ ]]; then
        \builtin continue
      fi
      if [[ ! "$__CODEX_SNAPSHOT_NAME" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
        \builtin continue
      fi
      if ! __CODEX_SNAPSHOT_EXPORT_DECLARATION=$(\builtin declare -xp "$__CODEX_SNAPSHOT_NAME" 2>/dev/null); then
        \builtin continue
      fi
      __CODEX_SNAPSHOT_EXPORT_REST=${__CODEX_SNAPSHOT_EXPORT_DECLARATION#declare }
      __CODEX_SNAPSHOT_EXPORT_FLAGS=${__CODEX_SNAPSHOT_EXPORT_REST%% *}
      # Exported environment bytes must not replay Bash-only attributes that
      # can reject or transform a later explicit policy restore. Bash arrays
      # are not representable in the child-process environment.
      case "$__CODEX_SNAPSHOT_EXPORT_FLAGS" in
        *a*|*A*) \builtin continue ;;
      esac
      __CODEX_SNAPSHOT_EXPORT_ASSIGNMENT=${__CODEX_SNAPSHOT_EXPORT_REST#* }
      \builtin printf '%s export %s\n' '\builtin' "$__CODEX_SNAPSHOT_EXPORT_ASSIGNMENT"
    done < <(\builtin compgen -e)
    \builtin true
  else
    [[ 1 == 0 ]]
  fi
else
  [[ 1 == 0 ]]
fi
"##;
    script.replace("EXCLUDED_EXPORTS", &excluded)
}

fn sh_snapshot_script() -> String {
    let excluded = excluded_exports_regex();
    let script = r##"case "$(\command printf '%s' __CODEX_SNAPSHOT_COMMAND_OK)" in
  __CODEX_SNAPSHOT_COMMAND_OK)
    \command test -z "$ENV" || \command test ! -r "$ENV" || \command . "$ENV"
    case "$(\command printf '%s' __CODEX_SNAPSHOT_COMMAND_OK)" in
      __CODEX_SNAPSHOT_COMMAND_OK)
        if \command -v typeset >/dev/null 2>&1; then
          __CODEX_SNAPSHOT_FUNCTIONS=$(\command typeset -f)
        elif \command -v declare >/dev/null 2>&1; then
          __CODEX_SNAPSHOT_FUNCTIONS=$(\command declare -f)
        else
          __CODEX_SNAPSHOT_FUNCTIONS=
        fi
        __CODEX_SNAPSHOT_ALIAS_LINES=$(\command alias 2>/dev/null) || __CODEX_SNAPSHOT_ALIAS_LINES=
        \command unalias -a 2>/dev/null || \command true
        \command printf '%s\n' '# Snapshot file'
        \command printf '%s\n' '# Codex shell snapshot format: 3'
        \command printf '%s\n' '# Unset all aliases to avoid conflicts with functions'
        \command printf '%s\n' '\command unalias -a 2>/dev/null || \command true'
        \command printf '%s\n' '# setopts'
        if __CODEX_SNAPSHOT_SH_OPTS_OUTPUT=$(\command set -o 2>/dev/null); then
          __CODEX_SNAPSHOT_SH_OPTS=$(\command printf '%s\n' "$__CODEX_SNAPSHOT_SH_OPTS_OUTPUT" | \command awk '$2=="on"{print $1}')
          if \command [ -n "$__CODEX_SNAPSHOT_SH_OPTS" ]; then
            for __CODEX_SNAPSHOT_SH_OPT in $__CODEX_SNAPSHOT_SH_OPTS; do
              \command printf '%s set -o %s\n' '\command' "$__CODEX_SNAPSHOT_SH_OPT"
            done
          fi
        fi
        \command printf '\n'
        \command printf '%s\n' '# Functions'
        __CODEX_SNAPSHOT_FUNCTIONS_ESCAPED=$(\command printf '%s' "$__CODEX_SNAPSHOT_FUNCTIONS" | \command sed "s/'/'\"'\"'/g")
        \command printf "__CODEX_SNAPSHOT_FUNCTIONS='%s'\n" "$__CODEX_SNAPSHOT_FUNCTIONS_ESCAPED"
        \command printf '\n'
        \command printf '%s\n' '# aliases'
        if \command [ -n "$__CODEX_SNAPSHOT_ALIAS_LINES" ]; then
          __CODEX_SNAPSHOT_ALIASES=$(
            \command printf '%s\n' "$__CODEX_SNAPSHOT_ALIAS_LINES" |
              while IFS= \command read -r __CODEX_SNAPSHOT_ALIAS_LINE; do
                \command printf '%s alias %s\n' '\command' "$__CODEX_SNAPSHOT_ALIAS_LINE"
              done
          )
        else
          __CODEX_SNAPSHOT_ALIASES=
        fi
        __CODEX_SNAPSHOT_ALIASES_ESCAPED=$(\command printf '%s' "$__CODEX_SNAPSHOT_ALIASES" | \command sed "s/'/'\"'\"'/g")
        \command printf "__CODEX_SNAPSHOT_ALIASES='%s'\n" "$__CODEX_SNAPSHOT_ALIASES_ESCAPED"
        \command printf '\n'
        \command printf '%s\n' '# exports'
        if __CODEX_SNAPSHOT_EXPORT_OUTPUT=$(\command export -p 2>/dev/null); then
          __CODEX_SNAPSHOT_EXPORT_LINES=$(\command printf '%s\n' "$__CODEX_SNAPSHOT_EXPORT_OUTPUT" | \command awk '
/^(export|declare -x|typeset -x) / {
  line=$0
  name=line
  sub(/^(export|declare -x|typeset -x) /, "", name)
  sub(/=.*/, "", name)
  if (name ~ /^__CODEX_SNAPSHOT_/ || name ~ /^(EXCLUDED_EXPORTS)$/) {
    next
  }
  if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) {
    print line
  }
}')
          if \command [ -n "$__CODEX_SNAPSHOT_EXPORT_LINES" ]; then
            \command printf '%s\n' "$__CODEX_SNAPSHOT_EXPORT_LINES" |
              while IFS= \command read -r __CODEX_SNAPSHOT_EXPORT_LINE; do
                \command printf '%s %s\n' '\command' "$__CODEX_SNAPSHOT_EXPORT_LINE"
              done
          fi
        else
          \command env | \command sort | while IFS='=' \command read -r __CODEX_SNAPSHOT_KEY __CODEX_SNAPSHOT_VALUE; do
            case "$__CODEX_SNAPSHOT_KEY" in
              ""|[0-9]*|*[!A-Za-z0-9_]*|__CODEX_SNAPSHOT_*|EXCLUDED_EXPORTS) \command continue ;;
            esac
            __CODEX_SNAPSHOT_ESCAPED=$(\command printf "%s" "$__CODEX_SNAPSHOT_VALUE" | \command sed "s/'/'\"'\"'/g")
            \command printf "%s export %s='%s'\n" '\command' "$__CODEX_SNAPSHOT_KEY" "$__CODEX_SNAPSHOT_ESCAPED"
          done
        fi
        ;;
      *)
        \exit 86
        ;;
    esac
    ;;
  *)
    \exit 86
    ;;
esac
"##;
    script.replace("EXCLUDED_EXPORTS", &excluded)
}

fn powershell_snapshot_script() -> &'static str {
    r##"$ErrorActionPreference = 'Stop'
Write-Output '# Snapshot file'
Write-Output '# Unset all aliases to avoid conflicts with functions'
Write-Output 'Remove-Item Alias:* -ErrorAction SilentlyContinue'
Write-Output '# Functions'
Get-ChildItem Function: | ForEach-Object {
    "function {0} {{`n{1}`n}}" -f $_.Name, $_.Definition
}
Write-Output ''
$aliases = Get-Alias
Write-Output '# aliases'
$aliases | ForEach-Object {
    "Set-Alias -Name {0} -Value {1}" -f $_.Name, $_.Definition
}
Write-Output ''
$envVars = Get-ChildItem Env:
Write-Output '# exports'
$envVars | ForEach-Object {
    $escaped = $_.Value -replace "'", "''"
    "`$env:{0}='{1}'" -f $_.Name, $escaped
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
        "sh" | "ps1" => Some(
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
