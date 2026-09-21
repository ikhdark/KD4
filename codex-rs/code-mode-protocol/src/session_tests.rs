use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::CellId;
use super::StartedCell;
use crate::FunctionCallOutputContentItem;
use crate::RuntimeResponse;

#[tokio::test]
async fn started_cell_preserves_successful_initial_responses() {
    let cell_id = CellId::new("cell-success".to_string());
    let expected = RuntimeResponse::Result {
        output_loss: None,
        cell_id: cell_id.clone(),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "delivered output".to_string(),
        }],
        error_text: None,
    };
    let (response_tx, response_rx) = oneshot::channel();
    response_tx.send(expected.clone()).expect("receiver open");
    let direct = StartedCell::new(cell_id.clone(), response_rx);
    let (response_tx, response_rx) = oneshot::channel();
    response_tx
        .send(Ok(expected.clone()))
        .expect("receiver open");
    let remote = StartedCell::from_result_receiver(cell_id.clone(), response_rx);

    for started in [direct, remote] {
        assert_eq!(started.cell_id, cell_id);
        assert_eq!(started.initial_response().await, Ok(expected.clone()));
    }
}

#[tokio::test]
async fn started_cell_reports_disconnected_initial_response_senders() {
    let cell_id = CellId::new("cell-disconnected".to_string());
    let (response_tx, response_rx) = oneshot::channel();
    drop(response_tx);
    let direct = StartedCell::new(cell_id.clone(), response_rx);
    let (response_tx, response_rx) = oneshot::channel();
    drop(response_tx);
    let remote = StartedCell::from_result_receiver(cell_id, response_rx);

    for started in [direct, remote] {
        assert_eq!(
            started.initial_response().await,
            Err("exec runtime ended unexpectedly".to_string())
        );
    }
}

#[tokio::test]
async fn started_cell_preserves_remote_initial_response_errors() {
    let (response_tx, response_rx) = oneshot::channel();
    response_tx
        .send(Err("remote runtime failed".to_string()))
        .expect("initial response receiver should be open");
    let started = StartedCell::from_result_receiver(CellId::new("1".to_string()), response_rx);

    assert_eq!(
        started.initial_response().await,
        Err("remote runtime failed".to_string())
    );
}
