use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
#[cfg(windows)]
use anyhow::bail;

pub(super) fn runtime_dir() -> PathBuf {
    env::temp_dir().join("codex-app-server-test-client")
}

pub(super) fn add_codex_parent_to_path(cmd: &mut Command, codex_bin: &Path) -> Result<()> {
    let Some(codex_bin_parent) = codex_bin
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    let mut paths = vec![codex_bin_parent.to_path_buf()];
    if let Some(existing_path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing_path));
    }
    let path = env::join_paths(paths).context("failed to build PATH for app-server child")?;
    cmd.env("PATH", path);
    Ok(())
}

pub(super) fn build_serve_command(
    codex_bin: &Path,
    config_overrides: &[String],
    listen: &str,
) -> Result<Command> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut command = build_direct_codex_command(codex_bin, config_overrides)?;
        command.arg("--listen").arg(listen);
        command.creation_flags(0x0000_0200 | 0x0800_0000); // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        Ok(command)
    }

    #[cfg(not(windows))]
    {
        let mut cmdline = format!(
            "tail -f /dev/null | RUST_BACKTRACE=full RUST_LOG=warn,codex_=trace {}",
            shell_quote(&codex_bin.display().to_string())
        );
        for override_kv in config_overrides {
            cmdline.push_str(&format!(" --config {}", shell_quote(override_kv)));
        }
        cmdline.push_str(&format!(" app-server --listen {}", shell_quote(listen)));

        let mut command = Command::new("nohup");
        command.arg("sh").arg("-c").arg(cmdline);
        Ok(command)
    }
}

#[cfg(windows)]
fn build_direct_codex_command(codex_bin: &Path, config_overrides: &[String]) -> Result<Command> {
    let mut command = Command::new(codex_bin);
    add_codex_parent_to_path(&mut command, codex_bin)?;
    command
        .env("RUST_BACKTRACE", "full")
        .env("RUST_LOG", "warn,codex_=trace");
    for override_kv in config_overrides {
        command.arg("--config").arg(override_kv);
    }
    command.arg("app-server");
    Ok(command)
}

#[cfg(windows)]
fn build_windows_serve_preflight_command(
    codex_bin: &Path,
    config_overrides: &[String],
) -> Result<Command> {
    let mut command = build_direct_codex_command(codex_bin, config_overrides)?;
    command
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub(super) fn preflight_serve_restart(codex_bin: &Path, config_overrides: &[String]) -> Result<()> {
    #[cfg(windows)]
    {
        let status = build_windows_serve_preflight_command(codex_bin, config_overrides)?
            .status()
            .with_context(|| {
                format!(
                    "failed to launch `{}` for app-server restart preflight; existing listeners were not changed",
                    codex_bin.display()
                )
            })?;
        if !status.success() {
            bail!(
                "`{} app-server --help` failed with {status}; existing listeners were not changed",
                codex_bin.display()
            );
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let _ = (codex_bin, config_overrides);
        Ok(())
    }
}

pub(super) fn start_prepared_serve<T>(
    kill: bool,
    preflight: impl FnOnce() -> Result<()>,
    kill_existing: impl FnOnce() -> Result<()>,
    spawn: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if kill {
        preflight()?;
        kill_existing()?;
    }
    spawn()
}

#[cfg(windows)]
pub(super) fn listener_pids_on_port(port: u16) -> Result<Vec<u32>> {
    let output = Command::new("netstat")
        .arg("-ano")
        .arg("-p")
        .arg("tcp")
        .output()
        .with_context(|| format!("failed to run netstat for port {port}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_windows_listener_pids(
        &String::from_utf8_lossy(&output.stdout),
        port,
    ))
}

#[cfg(not(windows))]
pub(super) fn listener_pids_on_port(port: u16) -> Result<Vec<u32>> {
    let output = Command::new("lsof")
        .arg("-nP")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output()
        .with_context(|| format!("failed to run lsof for port {port}"))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_pid_lines(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(windows)]
fn parse_windows_listener_pids(output: &str, port: u16) -> Vec<u32> {
    sorted_unique_pids(output.lines().filter_map(|line| {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        let [protocol, local_address, _foreign_address, state, pid] = columns.as_slice() else {
            return None;
        };
        if !protocol.eq_ignore_ascii_case("TCP")
            || !state.eq_ignore_ascii_case("LISTENING")
            || !address_has_port(local_address, port)
        {
            return None;
        }
        pid.parse::<u32>().ok()
    }))
}

#[cfg(windows)]
fn address_has_port(address: &str, port: u16) -> bool {
    address
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        == Some(port)
}

#[cfg(any(not(windows), test))]
fn parse_pid_lines(output: &str) -> Vec<u32> {
    sorted_unique_pids(output.lines().filter_map(|line| line.trim().parse().ok()))
}

fn sorted_unique_pids(pids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut pids = pids.into_iter().collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(windows)]
pub(super) fn terminate_process(pid: u32, force: bool) -> Result<std::process::ExitStatus> {
    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        command.arg("/F");
    }
    command.status().with_context(|| {
        format!(
            "failed to {} pid {pid}",
            if force {
                "force terminate"
            } else {
                "terminate"
            }
        )
    })
}

#[cfg(not(windows))]
pub(super) fn terminate_process(pid: u32, force: bool) -> Result<std::process::ExitStatus> {
    let mut command = Command::new("kill");
    if force {
        command.arg("-9");
    }
    command.arg(pid.to_string());
    command.status().with_context(|| {
        format!(
            "failed to {} pid {pid}",
            if force {
                "force terminate"
            } else {
                "terminate"
            }
        )
    })
}

pub(super) fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::cell::RefCell;

    #[test]
    fn runtime_dir_uses_the_platform_temp_directory() {
        assert_eq!(
            runtime_dir(),
            env::temp_dir().join("codex-app-server-test-client")
        );
    }

    #[test]
    fn codex_parent_is_prepended_using_platform_path_rules() -> Result<()> {
        let codex_bin = env::temp_dir().join("codex-bin").join(if cfg!(windows) {
            "codex.exe"
        } else {
            "codex"
        });
        let mut command = Command::new(&codex_bin);

        add_codex_parent_to_path(&mut command, &codex_bin)?;

        let configured_path = command
            .get_envs()
            .find_map(|(name, value)| {
                name.to_string_lossy()
                    .eq_ignore_ascii_case("PATH")
                    .then_some(value)
            })
            .flatten()
            .expect("PATH override");
        let configured_paths = env::split_paths(configured_path).collect::<Vec<_>>();
        let expected_existing_paths = env::var_os("PATH")
            .map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();

        assert_eq!(
            configured_paths.first().map(PathBuf::as_path),
            codex_bin.parent()
        );
        assert_eq!(&configured_paths[1..], expected_existing_paths);
        Ok(())
    }

    #[test]
    fn bare_codex_name_does_not_add_an_empty_path_entry() -> Result<()> {
        let mut command = Command::new("codex");

        add_codex_parent_to_path(&mut command, Path::new("codex"))?;

        assert!(
            command
                .get_envs()
                .all(|(name, _)| !name.to_string_lossy().eq_ignore_ascii_case("PATH"))
        );
        Ok(())
    }

    #[test]
    fn pid_collection_is_sorted_and_deduplicated() {
        assert_eq!(sorted_unique_pids([9, 4, 9, 2]), vec![2, 4, 9]);
    }

    #[test]
    fn pid_line_parser_ignores_invalid_values_and_deduplicates() {
        assert_eq!(parse_pid_lines("9\ninvalid\n4\n9\n"), vec![4, 9]);
    }

    #[test]
    fn shell_quote_preserves_single_quotes() {
        assert_eq!(shell_quote("alpha'beta"), "'alpha'\\''beta'");
    }

    #[test]
    fn restart_preflight_failure_does_not_kill_or_spawn() {
        let calls = RefCell::new(Vec::new());

        let result: Result<()> = start_prepared_serve(
            true,
            || {
                calls.borrow_mut().push("preflight");
                anyhow::bail!("not restartable")
            },
            || {
                calls.borrow_mut().push("kill");
                Ok(())
            },
            || {
                calls.borrow_mut().push("spawn");
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(*calls.borrow(), vec!["preflight"]);
    }

    #[test]
    fn restart_preflights_before_kill_and_spawn() -> Result<()> {
        let calls = RefCell::new(Vec::new());

        let pid = start_prepared_serve(
            true,
            || {
                calls.borrow_mut().push("preflight");
                Ok(())
            },
            || {
                calls.borrow_mut().push("kill");
                Ok(())
            },
            || {
                calls.borrow_mut().push("spawn");
                Ok(42)
            },
        )?;

        assert_eq!(pid, 42);
        assert_eq!(*calls.borrow(), vec!["preflight", "kill", "spawn"]);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_serve_uses_codex_directly_without_unix_launchers() -> Result<()> {
        let codex_bin = Path::new(r"C:\local codex\codex.exe");
        let command = build_serve_command(
            codex_bin,
            &["model_provider=local test".to_string()],
            "ws://127.0.0.1:4222",
        )?;

        assert_eq!(command.get_program(), codex_bin.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                "--config",
                "model_provider=local test",
                "app-server",
                "--listen",
                "ws://127.0.0.1:4222",
            ]
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_restart_preflight_checks_the_same_codex_app_server() -> Result<()> {
        let codex_bin = Path::new(r"C:\local codex\codex.exe");
        let command = build_windows_serve_preflight_command(
            codex_bin,
            &["model_provider=local test".to_string()],
        )?;

        assert_eq!(command.get_program(), codex_bin.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                "--config",
                "model_provider=local test",
                "app-server",
                "--help",
            ]
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_serve_keeps_the_nohup_shell_launcher() -> Result<()> {
        let command = build_serve_command(
            Path::new("/tmp/local codex/codex"),
            &["model_provider=local test".to_string()],
            "ws://127.0.0.1:4222",
        )?;

        assert_eq!(command.get_program(), "nohup");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                "sh",
                "-c",
                "tail -f /dev/null | RUST_BACKTRACE=full RUST_LOG=warn,codex_=trace '/tmp/local codex/codex' --config 'model_provider=local test' app-server --listen 'ws://127.0.0.1:4222'",
            ]
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_netstat_parser_keeps_only_matching_tcp_listeners() {
        let output = r#"
  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:4222           0.0.0.0:0              LISTENING       400
  TCP    [::]:4222              [::]:0                 LISTENING       200
  TCP    127.0.0.1:4222         127.0.0.1:50000        ESTABLISHED     300
  TCP    127.0.0.1:14222        0.0.0.0:0              LISTENING       500
  tcp    127.0.0.1:4222         0.0.0.0:0              listening       400
  UDP    0.0.0.0:4222           *:*                                    600
"#;

        assert_eq!(parse_windows_listener_pids(output, 4222), vec![200, 400]);
    }
}
