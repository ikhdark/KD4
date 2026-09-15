use super::*;
use pretty_assertions::assert_eq;

fn foreign_cwd() -> PathUri {
    let uri = match PathConvention::native() {
        PathConvention::Windows => "file:///usr/local/src",
        PathConvention::Posix => "file:///C:/workspace/src",
    };
    PathUri::parse(uri).expect("valid foreign cwd")
}

#[test]
fn native_read_builder_resolves_relative_path() {
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    let cwd = test_path_buf("/workspace").abs();
    let item = build_command_execution_begin_item(&ExecCommandBeginEvent {
        call_id: "read-1".into(),
        process_id: None,
        turn_id: "turn-1".into(),
        started_at_ms: 0,
        command: vec!["cat".into(), "file.txt".into()],
        cwd: cwd.into(),
        parsed_cmd: vec![ParsedCommand::Read {
            cmd: "cat file.txt".into(),
            name: "file.txt".into(),
            path: PathBuf::from("file.txt"),
        }],
        source: codex_protocol::protocol::ExecCommandSource::Agent,
        interaction_input: None,
    });
    let ThreadItem::CommandExecution {
        command_actions, ..
    } = item
    else {
        panic!("command item");
    };
    assert_eq!(
        command_actions,
        vec![CommandAction::Read {
            command: "cat file.txt".into(),
            name: "file.txt".into(),
            path: test_path_buf("/workspace/file.txt").abs(),
        }]
    );
}

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
            "//server/share/Program Files/tool.exe",
            r#""//server/share/Program Files/tool.exe" "argument with space""#,
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
    let cwd = foreign_cwd();

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
