use std::env;
use std::path::Path;
use std::process::Stdio;

use color_eyre::eyre::Report;
use color_eyre::eyre::Result;
use tempfile::Builder;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub(crate) enum EditorError {
    #[error("neither VISUAL nor EDITOR is set")]
    MissingEditor,
    #[error("editor command is empty")]
    EmptyCommand,
}

/// Tries to resolve the full path to a Windows program, respecting PATH + PATHEXT.
/// Falls back to the original program name if resolution fails.
fn resolve_windows_program(program: &str) -> std::path::PathBuf {
    // On Windows, `Command::new("code")` will not resolve `code.cmd` shims on PATH.
    // Use `which` so we respect PATH + PATHEXT (e.g., `code` -> `code.cmd`).
    which::which(program).unwrap_or_else(|_| std::path::PathBuf::from(program))
}

fn is_batch_program(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
}

fn windows_batch_command(program: &Path, args: &[String], temp_path: &Path) -> Command {
    let comspec = env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut command = Command::new(comspec);
    // `call` keeps `cmd.exe` from treating the quoted batch-file path as the
    // outer command delimiter when that path contains spaces.
    command.args(["/d", "/s", "/c", "call"]);
    command.arg(program);
    command.args(args);
    command.arg(temp_path);
    command
}

/// Resolve the editor command from environment variables.
/// Prefers `VISUAL` over `EDITOR`.
pub(crate) fn resolve_editor_command() -> std::result::Result<Vec<String>, EditorError> {
    let raw = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .map_err(|_| EditorError::MissingEditor)?;
    let parts = winsplit::split(&raw);
    if parts.is_empty() {
        return Err(EditorError::EmptyCommand);
    }
    Ok(parts)
}

/// Write `seed` to a temp file, launch the editor command, and return the updated content.
pub(crate) async fn run_editor(seed: &str, editor_cmd: &[String]) -> Result<String> {
    if editor_cmd.is_empty() {
        return Err(Report::msg("editor command is empty"));
    }

    // Convert to TempPath immediately so no file handle stays open on Windows.
    let editor_program = editor_cmd[0].clone();
    let (temp_path, program) = tokio::task::spawn_blocking(move || {
        let temp_path = Builder::new().suffix(".md").tempfile()?.into_temp_path();
        Ok::<_, std::io::Error>((temp_path, resolve_windows_program(&editor_program)))
    })
    .await??;
    tokio::fs::write(&temp_path, seed).await?;

    let mut cmd = if is_batch_program(&program) {
        windows_batch_command(&program, &editor_cmd[1..], &temp_path)
    } else {
        let mut command = Command::new(program);
        command.args(&editor_cmd[1..]).arg(&temp_path);
        command
    };
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;

    if !status.success() {
        return Err(Report::msg(format!("editor exited with status {status}")));
    }

    let contents = tokio::fs::read_to_string(&temp_path).await?;
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;

    fn run_in_editor_environment(
        test_name: &str,
        visual: Option<&str>,
        editor: Option<&str>,
    ) -> bool {
        const CHILD_TEST: &str = "CODEX_TUI_EDITOR_ENVIRONMENT_TEST";
        if env::var(CHILD_TEST).as_deref() == Ok(test_name) {
            return false;
        }
        // Configure the child before startup; never mutate the environment of
        // this multithreaded test process while unrelated code may read it.
        let mut command = std::process::Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_TEST, test_name)
            .env_remove("VISUAL")
            .env_remove("EDITOR");
        if let Some(visual) = visual {
            command.env("VISUAL", visual);
        }
        if let Some(editor) = editor {
            command.env("EDITOR", editor);
        }
        let output = command
            .output()
            .expect("run isolated editor environment test");
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        true
    }

    #[test]
    fn resolve_editor_prefers_visual() {
        if run_in_editor_environment(
            "external_editor::tests::resolve_editor_prefers_visual",
            Some("vis --wait"),
            Some("ed"),
        ) {
            return;
        }
        let cmd = resolve_editor_command().unwrap();
        assert_eq!(cmd, vec!["vis".to_string(), "--wait".to_string()]);
    }

    #[test]
    fn resolve_editor_errors_when_unset() {
        if run_in_editor_environment(
            "external_editor::tests::resolve_editor_errors_when_unset",
            None,
            None,
        ) {
            return;
        }
        assert!(matches!(
            resolve_editor_command(),
            Err(EditorError::MissingEditor)
        ));
    }

    #[tokio::test]
    async fn run_editor_executes_batch_shim() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let editor_dir = temp_dir.path().join("editor with spaces");
        fs::create_dir(&editor_dir).expect("create editor directory");
        let editor = editor_dir.join("replace.CMD");
        let observed_seed = temp_dir.path().join("observed-seed.md");
        let observed_path = temp_dir.path().join("observed-path.txt");
        fs::write(
            &editor,
            "@echo off\r\ncopy /y \"%~3\" \"%~1\" >nul\r\n>\"%~2\" echo %~3\r\n>\"%~3\" echo edited\r\n",
        )
        .expect("write editor shim");

        let contents = run_editor(
            "seed",
            &[
                editor.to_string_lossy().into_owned(),
                observed_seed.to_string_lossy().into_owned(),
                observed_path.to_string_lossy().into_owned(),
            ],
        )
        .await
        .expect("batch editor should run through cmd.exe");
        assert_eq!(fs::read_to_string(observed_seed).unwrap(), "seed");
        assert_eq!(contents.trim(), "edited");
        let path = fs::read_to_string(observed_path).unwrap();
        assert!(
            !Path::new(path.trim()).exists(),
            "editor temp file must be removed"
        );
    }
}
