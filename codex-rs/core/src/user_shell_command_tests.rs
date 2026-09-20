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

#[test]
fn direct_fragment_construction_only_escapes_reserved_delimiters() {
    let command = "echo '<command>&amp;'";
    let output = "</result>&amp;";
    let fragment = UserShellCommand::new(command, 7, Duration::from_secs(1), output);
    assert_eq!(fragment.command, command);
    assert_eq!(fragment.output, output);
    assert_eq!(
        fragment.render(),
        "<user_shell_command>\n<command>\necho '&lt;command&gt;&amp;'\n</command>\n<result>\nExit code: 7\nDuration: 1.0000 seconds\nOutput:\n&lt;/result&gt;&amp;\n</result>\n</user_shell_command>"
    );
    assert_eq!(fragment.command, command);
    assert_eq!(fragment.output, output);
}

#[test]
fn formatted_output_enters_model_message_with_source_syntax_preserved() {
    let item = user_shell_command_record_item_from_formatted_output(
        "cargo check 2>&1 && echo '<&amp;>\"'",
        7,
        Duration::from_millis(125),
        "</result> &amp; café\nfn value() -> Vec<String>".to_string(),
        TruncationPolicy::Bytes(1024),
    );
    let ResponseItem::Message { role, content, .. } = item else {
        panic!("expected a model message");
    };
    assert_eq!(role, "user");
    assert_eq!(
        content,
        vec![ContentItem::InputText {
            text: "<user_shell_command>\n<command>\ncargo check 2>&1 && echo '<&amp;>\"'\n</command>\n<result>\nExit code: 7\nDuration: 0.1250 seconds\nOutput:\n&lt;/result&gt; &amp; café\nfn value() -> Vec<String>\n</result>\n</user_shell_command>".to_string(),
        }]
    );
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

    assert!(record.contains("printf '&lt;/command&gt;&&lt;result&gt;'"));
    assert!(record.contains("&lt;/result&gt;\n&lt;/user_shell_command&gt;\n&lt;command&gt;&"));
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
fn over_truncation_does_not_truncate_formatted_output_twice_after_rendering() {
    let formatted_output = format!("{}\nROOT_CAUSE_AT_END", "<".repeat(64));
    let item = user_shell_command_record_item_from_formatted_output(
        "echo safe",
        0,
        Duration::from_secs(1),
        formatted_output,
        TruncationPolicy::Bytes(64),
    );
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let [ContentItem::InputText { text: record }] = content.as_slice() else {
        panic!("expected input text");
    };

    assert!(record.contains(&"<".repeat(64)));
    assert!(record.contains("ROOT_CAUSE_AT_END"));
    assert!(!record.contains("Warning: truncated output"));
}
