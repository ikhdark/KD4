use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::approx_token_count;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::fs as tokio_fs;

#[tokio::test]
async fn build_memory_tool_developer_instructions_renders_embedded_template() {
    let temp = tempdir().unwrap();
    let codex_home = AbsolutePathBuf::from_absolute_path(temp.path()).unwrap();
    let memories_dir = codex_home.join("memories");
    tokio_fs::create_dir_all(&memories_dir).await.unwrap();
    tokio_fs::write(
        memories_dir.join("memory_summary.md"),
        "Short memory summary for tests.",
    )
    .await
    .unwrap();

    let instructions = build_memory_tool_developer_instructions(&codex_home)
        .await
        .unwrap();

    assert!(instructions.contains(&format!(
        "- {}/memory_summary.md (already provided below; do NOT open again)",
        memories_dir.display()
    )));
    assert!(instructions.contains("Short memory summary for tests."));
    assert_eq!(
        instructions
            .matches("========= MEMORY_SUMMARY BEGINS =========")
            .count(),
        1
    );
}

#[tokio::test]
async fn memory_tool_developer_instructions_have_a_firm_total_budget() {
    assert!(
        include_str!("../templates/memories/read_path.compact.md").len() <= 4_000,
        "static memory guidance should stay compact"
    );

    let temp = tempdir().unwrap();
    let codex_home = AbsolutePathBuf::from_absolute_path(temp.path()).unwrap();
    let memories_dir = codex_home.join("memories");
    tokio_fs::create_dir_all(&memories_dir).await.unwrap();
    tokio_fs::write(
        memories_dir.join("memory_summary.md"),
        "memory-token ".repeat(5_000),
    )
    .await
    .unwrap();

    let instructions = build_memory_tool_developer_instructions(&codex_home)
        .await
        .unwrap();

    assert!(
        approx_token_count(&instructions) <= 2_000,
        "memory guidance and summary must stay within the combined budget"
    );
}

#[test]
fn memory_tool_budget_accounts_for_a_long_memory_root() {
    // Keep the fixed template itself below the combined budget while leaving
    // little enough room that a byte-only summary budget would overflow it.
    let base_path = format!("C:/{}", "nested/".repeat(50));
    let memory_summary = "memory-token ".repeat(5_000);
    let instructions =
        render_memory_tool_developer_instructions(base_path.as_str(), memory_summary.as_str())
            .expect("long but usable memory root should still render");

    assert!(instructions.contains(base_path.as_str()));
    assert!(
        approx_token_count(&instructions) <= crate::MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT
    );
}

#[test]
fn memory_tool_omits_guidance_when_fixed_context_exceeds_the_budget() {
    let base_path = "x".repeat(2_000);
    assert!(
        render_memory_tool_developer_instructions(base_path.as_str(), "memory summary").is_none()
    );
}
