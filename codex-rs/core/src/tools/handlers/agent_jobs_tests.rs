use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::watch;

#[test]
fn parse_csv_supports_quotes_and_commas() {
    let input = "id,name\n1,\"alpha, beta\"\n2,gamma\n";
    let (headers, rows) = parse_csv(input).expect("csv parse");
    assert_eq!(headers, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "alpha, beta".to_string()],
            vec!["2".to_string(), "gamma".to_string()]
        ]
    );
}

#[test]
fn csv_escape_quotes_when_needed() {
    assert_eq!(csv_escape("simple"), "simple");
    assert_eq!(csv_escape("a,b"), "\"a,b\"");
    assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
}

#[test]
fn render_instruction_template_expands_placeholders_and_escapes_braces() {
    let row = json!({
        "path": "src/lib.rs",
        "area": "test",
        "file path": "docs/readme.md",
    });
    let rendered = render_instruction_template(
        "Review {path} in {area}. Also see {file path}. Use {{literal}}.",
        &row,
    );
    assert_eq!(
        rendered,
        "Review src/lib.rs in test. Also see docs/readme.md. Use {literal}."
    );
}

#[test]
fn render_instruction_template_leaves_unknown_placeholders() {
    let row = json!({
        "path": "src/lib.rs",
    });
    let rendered = render_instruction_template("Check {path} then {missing}", &row);
    assert_eq!(rendered, "Check src/lib.rs then {missing}");
}

#[test]
fn ensure_unique_headers_rejects_duplicates() {
    let headers = vec!["path".to_string(), "path".to_string()];
    let Err(err) = ensure_unique_headers(headers.as_slice()) else {
        panic!("expected duplicate header error");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("csv header path is duplicated".to_string())
    );
}

#[test]
fn changed_agent_status_is_consumed_once() {
    let (status_tx, status_rx) = watch::channel(AgentStatus::Running);
    let mut item = ActiveJobItem {
        item_id: "item-1".to_string(),
        started_at: Instant::now(),
        status_rx: Some(status_rx),
    };

    status_tx
        .send(AgentStatus::Completed(Some("done".to_string())))
        .expect("status receiver should remain open");

    assert_eq!(
        take_changed_status(&mut item),
        Some(AgentStatus::Completed(Some("done".to_string())))
    );
    assert_eq!(take_changed_status(&mut item), None);
}

#[tokio::test]
async fn closed_status_receiver_uses_bounded_polling_fallback() {
    let (status_tx, status_rx) = watch::channel(AgentStatus::Running);
    drop(status_tx);
    let thread_id = ThreadId::new();
    let active_items = HashMap::from([(
        thread_id,
        ActiveJobItem {
            item_id: "item-closed".to_string(),
            started_at: Instant::now(),
            status_rx: Some(status_rx),
        },
    )]);
    let started = Instant::now();

    tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_status_change(&active_items),
    )
    .await
    .expect("closed watch fallback should remain bounded");

    assert!(started.elapsed() >= STATUS_POLL_INTERVAL);
}
