use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireWaitOutcome;
use codex_code_mode_protocol::host::WireWaitRequest;

use super::RemoteSession;

pub(super) fn public_cell_id(generation: u64, cell_id: &WireCellId) -> CellId {
    if generation == 1 {
        CellId::new(cell_id.as_str().to_string())
    } else {
        CellId::new(format!("g{generation}:{}", cell_id.as_str()))
    }
}

pub(super) fn public_cell_id_from_protocol(generation: u64, cell_id: &CellId) -> CellId {
    public_cell_id(generation, &WireCellId::new(cell_id.as_str()))
}

pub(super) fn remote_cell_id(
    session: &RemoteSession,
    cell_id: &CellId,
) -> Result<WireCellId, String> {
    if session.generation == 1 {
        if cell_id.as_str().starts_with('g') && cell_id.as_str().contains(':') {
            return Err(format!(
                "cell {cell_id} belongs to a stale code-mode host generation"
            ));
        }
        return Ok(WireCellId::new(cell_id.as_str()));
    }
    let prefix = format!("g{}:", session.generation);
    let Some(remote_id) = cell_id.as_str().strip_prefix(&prefix) else {
        return Err(format!(
            "cell {cell_id} belongs to a stale code-mode host generation"
        ));
    };
    Ok(WireCellId::new(remote_id))
}

pub(super) fn remote_wait_request(
    session: &RemoteSession,
    request: WaitRequest,
) -> Result<WireWaitRequest, String> {
    Ok(WireWaitRequest {
        cell_id: remote_cell_id(session, &request.cell_id)?,
        yield_time_ms: request.yield_time_ms,
    })
}

pub(super) fn public_runtime_response(
    generation: u64,
    response: RuntimeResponse,
) -> RuntimeResponse {
    match response {
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => RuntimeResponse::Yielded {
            cell_id: public_cell_id_from_protocol(generation, &cell_id),
            content_items,
        },
        RuntimeResponse::ExplicitYield {
            cell_id,
            content_items,
        } => RuntimeResponse::ExplicitYield {
            cell_id: public_cell_id_from_protocol(generation, &cell_id),
            content_items,
        },
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => RuntimeResponse::Terminated {
            cell_id: public_cell_id_from_protocol(generation, &cell_id),
            content_items,
        },
        RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
            output_loss,
        } => RuntimeResponse::Result {
            cell_id: public_cell_id_from_protocol(generation, &cell_id),
            content_items,
            error_text,
            output_loss,
        },
    }
}

pub(super) fn public_wait_outcome(generation: u64, outcome: WaitOutcome) -> WaitOutcome {
    match outcome {
        WaitOutcome::LiveCell(response) => {
            WaitOutcome::LiveCell(public_runtime_response(generation, response))
        }
        WaitOutcome::MissingCell(response) => {
            WaitOutcome::MissingCell(public_runtime_response(generation, response))
        }
    }
}

pub(super) fn runtime_response_cell_id(response: &WireRuntimeResponse) -> &WireCellId {
    match response {
        WireRuntimeResponse::Yielded { cell_id, .. }
        | WireRuntimeResponse::ExplicitYield { cell_id, .. }
        | WireRuntimeResponse::Terminated { cell_id, .. }
        | WireRuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}

pub(super) fn wait_outcome_cell_id(outcome: &WireWaitOutcome) -> &WireCellId {
    match outcome {
        WireWaitOutcome::LiveCell(response) | WireWaitOutcome::MissingCell(response) => {
            runtime_response_cell_id(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_code_mode_protocol::OutputLoss;

    use super::*;

    #[test]
    fn public_result_preserves_output_loss_while_rewriting_cell_id() {
        let output_loss = OutputLoss {
            discarded_items: 2,
            discarded_bytes_lower_bound: 4096,
        };

        assert_eq!(
            public_runtime_response(
                2,
                RuntimeResponse::Result {
                    cell_id: CellId::new("7".to_string()),
                    content_items: Vec::new(),
                    error_text: None,
                    output_loss: Some(output_loss.clone()),
                },
            ),
            RuntimeResponse::Result {
                cell_id: CellId::new("g2:7".to_string()),
                content_items: Vec::new(),
                error_text: None,
                output_loss: Some(output_loss),
            }
        );
    }
}
