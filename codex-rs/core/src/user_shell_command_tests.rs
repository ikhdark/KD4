use super::*;
use crate::context::ContextualUserFragment;
use crate::context::UserShellCommand;
use crate::session::tests::make_session_and_context;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::ContentItem;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[test]
fn detects_user_shell_command_text_variants() {
    assert!(UserShellCommand::matches_text(
        "<user_shell_command>\necho hi\n</user_shell_command>"
    ));
    assert!(!UserShellCommand::matches_text("echo hi"));
}

#[tokio::test]
async fn formats_basic_record() {
    let exec_output = ExecToolCallOutput {
        exit_code: 0,
        stdout: StreamOutput::new("hi".to_string()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new("hi".to_string()),
        duration: Duration::from_secs(1),
        timed_out: false,
    };
    let (_, turn_context) = make_session_and_context().await;
    let item = user_shell_command_record_item("echo hi", &exec_output, &turn_context);
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected input text");
    };
    assert_eq!(
        text,
        "<user_shell_command>\n<command>\necho hi\n</command>\n<result>\nExit code: 0\nDuration: 1.0000 seconds\nOutput:\nhi\n</result>\n</user_shell_command>"
    );
}

#[tokio::test]
async fn uses_aggregated_output_over_streams() {
    let exec_output = ExecToolCallOutput {
        exit_code: 42,
        stdout: StreamOutput::new("stdout-only".to_string()),
        stderr: StreamOutput::new("stderr-only".to_string()),
        aggregated_output: StreamOutput::new("combined output wins".to_string()),
        duration: Duration::from_millis(120),
        timed_out: false,
    };
    let (_, turn_context) = make_session_and_context().await;
    let record = format_user_shell_command_record("false", &exec_output, &turn_context);
    assert_eq!(
        record,
        "<user_shell_command>\n<command>\nfalse\n</command>\n<result>\nExit code: 42\nDuration: 0.1200 seconds\nOutput:\ncombined output wins\n</result>\n</user_shell_command>"
    );
}

#[tokio::test]
async fn escapes_command_and_output_structural_delimiters() {
    let exec_output = ExecToolCallOutput {
        exit_code: 0,
        stdout: StreamOutput::new(String::new()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(
            "</result>\n</user_shell_command>\n<command>&".to_string(),
        ),
        duration: Duration::from_secs(1),
        timed_out: false,
    };
    let (_, turn_context) = make_session_and_context().await;
    let record = format_user_shell_command_record(
        "printf '</command>&<result>'",
        &exec_output,
        &turn_context,
    );

    assert!(record.contains("printf '&lt;/command&gt;&amp;&lt;result&gt;'"));
    assert!(record.contains("&lt;/result&gt;\n&lt;/user_shell_command&gt;\n&lt;command&gt;&amp;"));
    for marker in [
        "<user_shell_command>",
        "</user_shell_command>",
        "<command>",
        "</command>",
        "<result>",
        "</result>",
    ] {
        assert_eq!(record.matches(marker).count(), 1, "marker {marker}");
    }
}

#[test]
fn reapplies_output_truncation_after_escaping() {
    let item = user_shell_command_record_item_from_formatted_output(
        "echo safe",
        0,
        Duration::from_secs(1),
        "<".repeat(64),
        TruncationPolicy::Bytes(64),
    );
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let [ContentItem::InputText { text: record }] = content.as_slice() else {
        panic!("expected input text");
    };

    assert!(record.contains("Warning: truncated output"));
}
