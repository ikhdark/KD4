use super::*;
use pretty_assertions::assert_eq;

#[test]
fn windows_absolute_program_paths_use_windows_display_quoting() {
    for (program, expected) in [
        (
            r"C:\Program Files\tool.exe",
            r#""C:\Program Files\tool.exe" "argument with space""#,
        ),
        (
            r"C:/Program Files/tool.exe",
            r#""C:/Program Files/tool.exe" "argument with space""#,
        ),
        (
            r"\\server\share\Program Files\tool.exe",
            r#""\\server\share\Program Files\tool.exe" "argument with space""#,
        ),
        (
            r"\\?\C:\Program Files\tool.exe",
            r#""\\?\C:\Program Files\tool.exe" "argument with space""#,
        ),
    ] {
        let command = vec![program.to_string(), "argument with space".to_string()];

        assert_eq!(command_display_string(&command), expected);
    }
}

#[test]
fn windows_display_quoting_handles_empty_quotes_and_trailing_backslashes() {
    assert_eq!(quote_windows_display_arg(""), "\"\"");
    assert_eq!(
        quote_windows_display_arg("say \"hello\""),
        "\"say \\\"hello\\\"\""
    );
    assert_eq!(
        quote_windows_display_arg("C:\\path with space\\"),
        "\"C:\\path with space\\\\\""
    );
}

#[test]
fn non_absolute_or_already_quoted_programs_keep_posix_display() {
    for command in [
        vec!["/bin/bash".to_string(), "echo hi".to_string()],
        vec![
            "\"C:\\Program Files\\tool.exe\"".to_string(),
            "argument with space".to_string(),
        ],
        vec!["C:tool.exe".to_string(), "argument with space".to_string()],
        vec![
            r"\Windows\tool.exe".to_string(),
            "argument with space".to_string(),
        ],
        vec![r".\tool.exe".to_string(), "argument with space".to_string()],
        vec!["pwsh.exe".to_string(), "argument with space".to_string()],
        vec![
            "//server/share/tool.exe".to_string(),
            "argument with space".to_string(),
        ],
    ] {
        assert_eq!(
            command_display_string(&command),
            codex_shell_command::parse_command::shlex_join(&command)
        );
    }
}

#[test]
fn windows_display_preserves_the_existing_nul_placeholder() {
    let command = vec![r"C:\tool.exe".to_string(), "bad\0argument".to_string()];

    assert_eq!(
        command_display_string(&command),
        "<command included NUL byte>"
    );
}

#[test]
fn foreign_read_is_omitted_without_dropping_other_command_actions() {
    #[cfg(windows)]
    let cwd = PathUri::parse("file:///usr/local/src").expect("valid foreign POSIX cwd");
    #[cfg(not(windows))]
    let cwd = PathUri::parse("file:///C:/src").expect("valid foreign Windows cwd");
    let parsed_cmd = vec![
        ParsedCommand::Read {
            cmd: "cat file.txt".to_string(),
            name: "file.txt".to_string(),
            path: PathBuf::from("file.txt"),
        },
        ParsedCommand::ListFiles {
            cmd: "ls".to_string(),
            path: Some("subdir".to_string()),
        },
        ParsedCommand::Search {
            cmd: "rg needle".to_string(),
            query: Some("needle".to_string()),
            path: Some("src".to_string()),
        },
    ];

    assert_eq!(
        command_actions_for_path_uri(&parsed_cmd, &cwd),
        vec![
            CommandAction::ListFiles {
                command: "ls".to_string(),
                path: Some("subdir".to_string()),
            },
            CommandAction::Search {
                command: "rg needle".to_string(),
                query: Some("needle".to_string()),
                path: Some("src".to_string()),
            },
        ]
    );
}
