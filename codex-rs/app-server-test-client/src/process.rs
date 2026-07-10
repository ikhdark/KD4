use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

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
