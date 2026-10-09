use super::SkillLoadOutcome;
use super::SkillMetadata;
use super::build_implicit_skill_path_indexes;
use super::detect_implicit_skill_invocation_for_command;
use super::script_run_token;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn test_skill_metadata(skill_doc_path: AbsolutePathBuf) -> SkillMetadata {
    SkillMetadata {
        name: "test-skill".to_string(),
        description: "test".to_string(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: skill_doc_path,
        scope: codex_protocol::protocol::SkillScope::User,
        plugin_id: None,
    }
}

#[test]
fn script_run_detection_matches_runner_plus_extension() {
    for (tokens, expected) in [
        (vec!["python3", "-u", "scripts/fetch_comments.py"], Some("scripts/fetch_comments.py")),
        (vec!["python3", "-c", "print(1)"], None),
        (vec!["not-python", "scripts/fetch_comments.py"], None),
        (vec!["python3"], None),
        (vec!["PWSH.exe", "--", "scripts/check.PS1"], Some("scripts/check.PS1")),
    ] {
        let tokens = tokens.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(script_run_token(&tokens), expected, "{tokens:?}");
    }
}

#[test]
fn command_detection_resolves_script_and_document_paths() {
    let skill = test_skill_metadata(test_path_buf("/tmp/skill-test/SKILL.md").abs());
    let (scripts, docs) = build_implicit_skill_path_indexes(vec![skill.clone()]);
    let outcome = SkillLoadOutcome {
        implicit_skills_by_scripts_dir: Arc::new(scripts),
        implicit_skills_by_doc_path: Arc::new(docs),
        ..Default::default()
    };
    let doc = skill.path_to_skills_md.to_string_lossy().replace('\\', "/");
    let script = test_path_buf("/tmp/skill-test/scripts/fetch_comments.py")
        .to_string_lossy().replace('\\', "/");
    for (command, cwd, expected) in [
        (format!("cat \"{doc}\" | head"), "/tmp", Some(skill.clone())),
        (format!("nl -ba \"{doc}\""), "/tmp", Some(skill.clone())),
        ("python3 scripts/fetch_comments.py".to_string(), "/tmp/skill-test", Some(skill.clone())),
        (format!("python3 \"{script}\""), "/tmp/other", Some(skill)),
        ("python3 scripts/fetch_comments.py".to_string(), "/tmp/other", None),
        ("cat unrelated.md".to_string(), "/tmp/skill-test", None),
    ] {
        assert_eq!(
            detect_implicit_skill_invocation_for_command(&outcome, &command, &test_path_buf(cwd).abs()),
            expected,
            "{command} from {cwd}"
        );
    }
}
