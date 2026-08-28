use super::*;
use pretty_assertions::assert_eq;

#[test]
fn review_prompt_template_renders_base_branch_variant() {
    assert_eq!(
        render_review_prompt(
            &BASE_BRANCH_PROMPT_TEMPLATE,
            [("base_branch", "main"), ("merge_base_sha", "abc123")]
        ),
        "Review the code changes against the base branch 'main'. The merge base commit for this comparison is abc123. Run `git diff abc123` to inspect the changes relative to main. Provide prioritized, actionable findings."
    );
}

#[test]
fn review_prompt_template_renders_commit_variant() {
    assert_eq!(
        review_prompt(
            &ReviewTarget::Commit {
                sha: "deadbeef".to_string(),
                title: None,
            },
            &AbsolutePathBuf::current_dir().expect("cwd"),
        )
        .expect("commit prompt should render"),
        "Review the code changes introduced by commit deadbeef. Provide prioritized, actionable findings."
    );
}

#[test]
fn review_prompt_template_renders_commit_variant_with_title() {
    assert_eq!(
        review_prompt(
            &ReviewTarget::Commit {
                sha: "deadbeef".to_string(),
                title: Some("Fix bug".to_string()),
            },
            &AbsolutePathBuf::current_dir().expect("cwd"),
        )
        .expect("commit prompt should render"),
        "Review the code changes introduced by commit deadbeef (\"Fix bug\"). Provide prioritized, actionable findings."
    );
}

#[test]
fn review_rubric_stays_compact_without_losing_output_contracts() {
    assert!(
        REVIEW_PROMPT.len() <= 5_000,
        "review rubric grew to {} bytes",
        REVIEW_PROMPT.len()
    );
    for required in [
        "Return every qualifying issue",
        "[P0]",
        "\"priority\"",
        "\"code_location\"",
        "\"overall_correctness\"",
        "location must overlap the diff",
        "Do not generate a PR fix",
    ] {
        assert!(
            REVIEW_PROMPT.contains(required),
            "review rubric lost required contract: {required}"
        );
    }
}
