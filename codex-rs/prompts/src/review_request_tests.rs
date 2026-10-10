use super::*;
use pretty_assertions::assert_eq;

#[test]
fn review_prompt_template_renders_base_branch_variant() {
    assert_eq!(
        render_review_prompt(
            &BASE_BRANCH_PROMPT_TEMPLATE,
            [("base_branch", "main"), ("merge_base_sha", "abc123")]
        ),
        "Review the code changes against the base branch 'main'. The merge base commit for this comparison is abc123. Run `git diff abc123` to inspect tracked changes relative to main, including staged and unstaged edits. Provide prioritized, actionable findings."
    );
}

#[test]
fn review_prompt_template_renders_commit_variant() {
    let cwd = AbsolutePathBuf::current_dir().expect("cwd");
    for (title, expected) in [
        (None, "Review the code changes introduced by commit deadbeef. Provide prioritized, actionable findings."),
        (Some("Fix bug"), "Review the code changes introduced by commit deadbeef (\"Fix bug\"). Provide prioritized, actionable findings."),
    ] {
        assert_eq!(
            review_prompt(
                &ReviewTarget::Commit {
                    sha: "deadbeef".to_string(),
                    title: title.map(str::to_string),
                },
                &cwd,
            )
            .expect("commit prompt should render"),
            expected,
            "title: {title:?}"
        );
    }
}

#[test]
fn resolves_custom_review_without_losing_target_or_explicit_hint() {
    let cwd = AbsolutePathBuf::current_dir().expect("cwd");
    let target = ReviewTarget::Custom {
        instructions: " \n Review Unicode λ changes \t ".to_string(),
    };
    for hint in [None, Some("chosen label"), Some("")] {
        let resolved = resolve_review_request(
            ReviewRequest {
                target: target.clone(),
                user_facing_hint: hint.map(str::to_string),
            },
            &cwd,
        )
        .expect("valid review request");
        assert_eq!(resolved.prompt, "Review Unicode λ changes");
        assert_eq!(resolved.target, target);
        let expected_hint = hint.unwrap_or("Review Unicode λ changes");
        assert_eq!(resolved.user_facing_hint, expected_hint);
        let restored: ReviewRequest = resolved.into();
        assert_eq!(restored.target, target);
        assert_eq!(restored.user_facing_hint.as_deref(), Some(expected_hint));
    }
}

#[test]
fn rejects_blank_custom_review_even_with_a_user_facing_hint() {
    let cwd = AbsolutePathBuf::current_dir().expect("cwd");
    for instructions in ["", " \r\n\t", "\u{2003}"] {
        let error = resolve_review_request(
            ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: instructions.to_string(),
                },
                user_facing_hint: Some("not a review prompt".to_string()),
            },
            &cwd,
        )
        .expect_err("labels cannot make an empty review task executable");
        assert_eq!(error.to_string(), "Review prompt cannot be empty");
    }
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
        "including code outside the diff",
        "generated artifacts, schemas, and required source maps",
        "relevant tests assert the expected behavior and could catch a plausible regression",
        "build and test results with their scope and freshness",
        "do not treat missing validation evidence alone as a demonstrated defect",
        "Missing requested behavior and violations of preserved invariants qualify",
        "cite the affected callers or contracts in the body",
        "only when applicable requirements and preserved invariants are satisfied",
        "Intentional changes remain reportable when direct evidence establishes a defect or violation of an applicable requirement.",
        "Base correctness on defects, not urgency: a lower-priority defect still makes the patch incorrect.",
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
