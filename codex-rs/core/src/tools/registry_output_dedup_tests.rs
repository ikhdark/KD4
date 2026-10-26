use super::*;

#[tokio::test]
async fn cumulative_raw_identity_does_not_suppress_a_suffix_or_capped_packet() {
    for incomplete_packet in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let raw = format!("{}{}", "a".repeat(200_000), "b".repeat(100_000));
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(home.path(), "poll", raw.as_bytes()).await;
        let raw_id = artifact.artifact_id().unwrap().to_string();
        let mut canonical = CanonicalToolResult::bytes(raw.as_bytes()[200_000..].to_vec());
        canonical.complete = !incomplete_packet;
        let text = "process result and bounded useful suffix".to_string();
        let projection = project_model_output(ModelProjectionInput {
            fragments:vec![], non_text_tokens:0, non_text_bytes:0, outcome:ToolOutputOutcome::Success,
            essential_inline:serde_json::json!({"raw_output_artifact_id":raw_id,"session_id":42,"exit_code":0}),
            origin_call_id:"poll".into(), selection_facts:ProjectionSelectionFacts {
                mode:"all", available_fragments:0, selected_fragments:0, exact_duplicates_removed:0,
                selected_ids:vec![], omitted_inline_ids:vec![], partial_ids:vec![],
            },
            applied_token_limit:2_000, projected_text:text.clone(), preserved_content:vec![],
            codex_home:home.path().into(), thread_id:"poll".into(), tool_name:"write_stdin".into(),
            original_output_sha256:canonical.sha256.clone(), original_output_tokens:canonical.approximate_tokens,
            original_output_text:text.clone(), invocation_sha256:None, semantic_class:"command".into(),
            source_dependencies:Default::default(), projection_eligible:true, projection_truncated:true,
            canonical_artifact_required:true, predetermined_ranges:vec![], predetermined_json_pointers:vec![],
            canonical, original_response:ResponseInputItem::FunctionCallOutput {
                call_id:"poll".into(), output:FunctionCallOutputPayload::from_text(text),
            }, materialization:ProjectionMaterialization::CanonicalArtifact,
        }).await.expect("a recoverable cumulative capture must not erase the process result");
        let envelope = projection.bounded.envelope().unwrap();
        assert_eq!(envelope.result["essential"]["raw_output_artifact_id"], raw_id);
        assert_eq!(envelope.result["essential"]["session_id"], 42);
        assert_eq!(envelope.canonical_complete, !incomplete_packet);
        let candidate = projection.candidate.unwrap();
        assert_ne!(candidate.artifact_id, raw_id);
        let raw_snapshot = crate::tools::command_output_artifact::load_tool_output_snapshot(home.path(), "poll", &raw_id).await.unwrap();
        let raw_result = raw_snapshot.select(vec![crate::tools::command_output_artifact::ToolOutputSelector::Bytes { start:0,end:3 }], 2_000).await.unwrap();
        assert_eq!(raw_result.results[0].text.as_deref(), Some("aaa"));
        let packet = crate::tools::command_output_artifact::load_tool_output_snapshot(home.path(), "poll", &candidate.artifact_id).await.unwrap();
        let packet_result = packet.select(vec![crate::tools::command_output_artifact::ToolOutputSelector::Bytes { start:0,end:3 }], 2_000).await.unwrap();
        assert_eq!(packet_result.results[0].text.as_deref(), Some("bbb"));
        assert_eq!(packet_result.capture_complete, !incomplete_packet);
    }
}
