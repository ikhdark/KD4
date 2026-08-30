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

#[test]
fn guardian_execve_preserves_foreign_cwd_without_native_conversion() {
    let cwd = foreign_cwd();
    let assessment = GuardianAssessmentEvent {
        id: "review-1".to_string(),
        target_item_id: Some("call-1".to_string()),
        turn_id: "turn-1".to_string(),
        started_at_ms: 1,
        completed_at_ms: None,
        status: codex_protocol::protocol::GuardianAssessmentStatus::InProgress,
        risk_level: None,
        user_authorization: None,
        rationale: None,
        decision_source: None,
        action: GuardianAssessmentAction::Execve {
            source: codex_protocol::protocol::GuardianCommandSource::UnifiedExec,
            program: "cat".to_string(),
            argv: vec!["cat".to_string(), "file.txt".to_string()],
            cwd: cwd.clone().into(),
        },
    };

    let item = build_item_from_guardian_event(&assessment, CommandExecutionStatus::InProgress)
        .expect("guardian execve maps to a command item");

    assert_eq!(
        item,
        ThreadItem::CommandExecution {
            id: "call-1".to_string(),
            command: "cat file.txt".to_string(),
            cwd: cwd.into(),
            process_id: None,
            source: CommandExecutionSource::Agent,
            status: CommandExecutionStatus::InProgress,
            command_actions: Vec::new(),
            aggregated_output: None,
            exit_code: None,
            duration_ms: None,
        }
    );
}
