use super::*;
use pretty_assertions::assert_eq;

#[test]
fn scoped_prompt_keeps_the_captured_base_when_branch_moves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new(codex_git_utils::git_executable())
            .current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init"]);
    std::fs::write(root.join("source.rs"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "base"]);
    let original = git(&["rev-parse", "HEAD"]);
    git(&["branch", "review-base"]);
    std::fs::write(root.join("source.rs"), "changed\n").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "change"]);
    let cwd = AbsolutePathBuf::from_absolute_path(root).unwrap();
    let target = ReviewTarget::BaseBranch { branch: "review-base".into() };
    let scope = codex_git_utils::ReviewSourceScope::capture(root, &target).unwrap();
    git(&["branch", "-f", "review-base", "HEAD"]);
    let moved = git(&["rev-parse", "HEAD"]);
    assert_ne!(original, moved);
    assert_eq!(scope.comparison_revision(), Some(original.as_str()));
    let prompt = review_prompt_for_scope(&target, &cwd, &scope).unwrap();
    assert!(prompt.contains(&format!("git diff {original}")));
    assert!(!prompt.contains(&moved));
}

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
        (
            None,
            "Review the code changes introduced by commit deadbeef. Use `git show --diff-merges=first-parent deadbeef` so merge commits use the same first-parent comparison as the host scope. Provide prioritized, actionable findings.",
        ),
        (
            Some("Fix bug"),
            "Review the code changes introduced by commit deadbeef (\"Fix bug\"). Use `git show --diff-merges=first-parent deadbeef` so merge commits use the same first-parent comparison as the host scope. Provide prioritized, actionable findings.",
        ),
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
    let rubric = include_str!("../templates/review/rubric.md");
    assert!(
        rubric.len() <= 5_000,
        "review rubric grew to {} bytes",
        rubric.len()
    );
    for required in [
        "Shared scope, evidence, validation, and completion policy comes from the canonical base instructions",
        "This reviewer is source-read-only",
        "Direct builds, tests, benchmarks, and writes remain blocked",
        "Completed-review output schema",
        "Blocked-review output schema",
        "Return every finding in that JSON",
        "Return every qualifying issue",
        "Check generated/schema counterparts and added/edited tests for contract drift and plausible regressions",
        "Missing validation evidence alone is not a defect",
        "Missing requested behavior and violations of preserved invariants qualify",
        "cite the affected callers or contracts in the body",
        "only when applicable requirements and preserved invariants are satisfied",
        "Intentional changes remain reportable when direct evidence establishes a defect or violation of an applicable requirement.",
        "Base correctness on defects, not urgency: a lower-priority defect still makes the patch incorrect.",
        "[P0]",
        "\"priority\"",
        "\"code_location\"",
        "\"overall_correctness\"",
        "Locations must overlap the diff only for patch reviews",
        "Do not generate a PR fix",
    ] {
        assert!(
            rubric.contains(required),
            "review rubric lost required contract: {required}"
        );
    }
}

#[test]
fn review_prompt_composes_the_canonical_shared_policy_before_its_profile() {
    let shared_policy = codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
    let rubric = include_str!("../templates/review/rubric.md");
    assert_eq!(REVIEW_PROMPT, format!("{shared_policy}\n\n{rubric}"));
    assert_eq!(REVIEW_PROMPT.matches(shared_policy).count(), 1);
    assert!(!rubric.contains("Performance findings require"));
    assert!(!rubric.contains("For a broad audit or an explicit full-read request"));
}
