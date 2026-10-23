use super::*;

#[tokio::test]
async fn accepted_retention_handle_survives_terminal_packet_and_late_storage() {
    use crate::tools::command_output_artifact::{ToolOutputArtifactId, ReadToolOutputError,
        reserve_canonical_retention, create_canonical_output_artifact_with_id, load_tool_output_snapshot};
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let service = &session.services.code_mode_service;
    let cell = CellId::new("audit147-late-storage".into());
    service.record_cell_parent_call_id(&cell, "outer");
    let ordinal = service.admit_packet_call(&cell, 32).unwrap();
    let home = tempfile::tempdir().unwrap();
    let thread = session.thread_id.to_string();
    let id = ToolOutputArtifactId::new();
    let owner = reserve_canonical_retention(home.path(), &thread, id);
    assert!(service.record_packet_recovery(&cell, ordinal,
        serde_json::json!({"outcome":{"artifact_id":id.to_string(), "canonical_retention":"pending"}})));
    let (release, blocked) = tokio::sync::oneshot::channel();
    let (done, settled) = tokio::sync::oneshot::channel();
    let destination = home.path().to_owned();
    assert!(service.queue_packet_retention(&session, &cell, async move {
        blocked.await.unwrap();
        let canonical = codex_tools::CanonicalToolResult::json(serde_json::json!({"patch_receipt":"x".repeat(2_048)}));
        let artifact = create_canonical_output_artifact_with_id(&destination, &thread, &canonical, id).await;
        owner.settle(&artifact);
        done.send(artifact.complete).unwrap();
    }).is_none());
    let packet = service.finish_packet(cell.as_str(), true);
    assert_eq!(packet.nested_recovery[&ordinal]["outcome"]["artifact_id"], id.to_string());
    service.finish_cell_dispatch(&cell);
    assert!(matches!(load_tool_output_snapshot(home.path(), &session.thread_id.to_string(), &id.to_string()).await,
        Err(ReadToolOutputError::StillWriting)));
    release.send(()).unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(5), settled).await.unwrap().unwrap());
    assert!(load_tool_output_snapshot(home.path(), &session.thread_id.to_string(), &id.to_string()).await.is_ok());
    assert!(service.packet_admission.lock().unwrap().cells.get(cell.as_str()).is_none());
    drop(turn);
}
