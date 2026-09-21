#![allow(clippy::expect_used)]
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_execpolicy::Decision;
use codex_execpolicy::Error;
use codex_execpolicy::Evaluation;
use codex_execpolicy::MatchOptions;
use codex_execpolicy::NetworkRuleProtocol;
use codex_execpolicy::PatternToken;
use codex_execpolicy::Policy;
use codex_execpolicy::PolicyParser;
use codex_execpolicy::PrefixPattern;
use codex_execpolicy::PrefixRule;
use codex_execpolicy::RuleMatch;
use codex_execpolicy::RuleRef;
use codex_execpolicy::blocking_append_allow_prefix_rule;
use codex_execpolicy::blocking_append_network_rule;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn tokens(cmd: &[&str]) -> Vec<String> {
    cmd.iter().map(std::string::ToString::to_string).collect()
}

fn allow_all(_: &[String]) -> Decision {
    Decision::Allow
}

fn prompt_all(_: &[String]) -> Decision {
    Decision::Prompt
}

fn absolute_path(path: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(path.to_string()).expect("path should be absolute")
}

fn host_absolute_path(segments: &[&str]) -> String {
    let mut path = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
    for segment in segments {
        path.push(segment);
    }
    path.to_string_lossy().into_owned()
}

fn host_executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn starlark_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn rule_snapshots(rules: &[RuleRef]) -> Vec<PrefixRule> {
    rules.iter().map(|rule| rule.as_ref().clone()).collect()
}

#[test]
fn append_allow_prefix_rule_dedupes_existing_rule() -> Result<()> {
    let tmp = tempdir().context("create temp dir")?;
    let policy_path = tmp.path().join("rules").join("default.rules");
    let prefix = tokens(&["python3"]);

    blocking_append_allow_prefix_rule(&policy_path, &prefix)?;
    blocking_append_allow_prefix_rule(&policy_path, &prefix)?;

    let contents = fs::read_to_string(&policy_path).context("read policy")?;
    assert_eq!(
        contents,
        r#"prefix_rule(pattern=["python3"], decision="allow")
"#
    );
    Ok(())
}

#[test]
fn network_amendments_preserve_latest_decision_after_reload() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join("default.rules");
    for decision in [Decision::Forbidden, Decision::Allow, Decision::Forbidden] {
        blocking_append_network_rule(
            &path,
            "example.com",
            NetworkRuleProtocol::Https,
            decision,
            None,
        )?;
        let mut parser = PolicyParser::new();
        parser.parse("default.rules", &fs::read_to_string(&path)?)?;
        let expected = if decision == Decision::Allow {
            (tokens(&["example.com"]), Vec::new())
        } else {
            (Vec::new(), tokens(&["example.com"]))
        };
        assert_eq!(parser.build().compiled_network_domains(), expected);
    }
    Ok(())
}

#[test]
fn prefix_amendment_rejects_empty_tokens_before_creating_file() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join("rules/default.rules");
    for prefix in [tokens(&["", "status"]), tokens(&[" \t", "status"])] {
        let error = blocking_append_allow_prefix_rule(&path, &prefix).expect_err("invalid token");
        assert!(error.to_string().contains("tokens cannot be empty"));
        assert!(!path.parent().unwrap().exists());
    }
    Ok(())
}

#[test]
fn network_compilation_preserves_last_effective_order_across_protocols() -> Result<()> {
    let mut parser = PolicyParser::new();
    parser.parse(
        "network.rules",
        r#"
network_rule(host="a.test", protocol="http", decision="allow")
network_rule(host="b.test", protocol="http", decision="allow")
network_rule(host="a.test", protocol="https", decision="deny")
network_rule(host="c.test", protocol="http", decision="deny")
network_rule(host="b.test", protocol="https", decision="allow")
network_rule(host="d.test", protocol="http", decision="allow")
network_rule(host="a.test", protocol="https", decision="allow")
network_rule(host="b.test", protocol="https", decision="prompt")
"#,
    )?;
    assert_eq!(
        parser.build().compiled_network_domains(),
        (tokens(&["b.test", "d.test", "a.test"]), tokens(&["c.test"]))
    );
    Ok(())
}

#[test]
fn network_rule_validates_and_canonicalizes_host_identity() -> Result<()> {
    for invalid in [
        "example.com:abc",
        "garbage::host",
        "[example.com]",
        "[::1]:65536",
        "example.com:",
        "user@example.com",
        "[::1%]",
    ] {
        let mut parser = PolicyParser::new();
        let source =
            format!(r#"network_rule(host="{invalid}", protocol="https", decision="allow")"#);
        assert!(
            parser.parse("network.rules", &source).is_err(),
            "accepted {invalid}"
        );
    }
    for (raw, expected) in [
        ("Example.COM.:443", "example.com"),
        ("[2001:0DB8:0:0::1]:443", "2001:db8::1"),
        ("[fe80::1%25ETH0]", "fe80::1%eth0"),
        ("127.0.0.1:80", "127.0.0.1"),
    ] {
        let mut parser = PolicyParser::new();
        parser.parse(
            "network.rules",
            &format!(r#"network_rule(host="{raw}", protocol="https", decision="allow")"#),
        )?;
        assert_eq!(
            parser.build().compiled_network_domains(),
            (tokens(&[expected]), Vec::new())
        );
    }
    Ok(())
}

#[test]
fn host_executable_setter_uses_native_identity() -> Result<()> {
    let mut policy = Policy::empty();
    policy.add_prefix_rule(&tokens(&["git"]), Decision::Allow)?;
    let name = if cfg!(windows) { "Git.EXE" } else { "git" };
    let allowed = host_absolute_path(&["trusted", &host_executable_name("git")]);
    let denied = host_absolute_path(&["untrusted", &host_executable_name("git")]);
    policy.set_host_executable_paths(name.to_string(), vec![absolute_path(&allowed)]);
    let options = MatchOptions {
        resolve_host_executables: true,
    };
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&allowed]), &prompt_all, &options)
            .decision,
        Decision::Allow
    );
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&denied]), &prompt_all, &options)
            .decision,
        Decision::Prompt
    );
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn host_executable_resolution_preserves_unix_case_and_suffix() -> Result<()> {
    let mut policy = Policy::empty();
    policy.add_prefix_rule(&tokens(&["git"]), Decision::Allow)?;
    let options = MatchOptions {
        resolve_host_executables: true,
    };
    for program in ["/usr/bin/GIT", "/usr/bin/git.exe", "/usr/bin/GIT.EXE"] {
        assert_eq!(
            policy
                .check_with_options(&tokens(&[program]), &prompt_all, &options)
                .decision,
            Decision::Prompt
        );
    }
    assert_eq!(
        policy
            .check_with_options(&tokens(&["/usr/bin/git"]), &prompt_all, &options)
            .decision,
        Decision::Allow
    );
    Ok(())
}

#[test]
fn network_rules_compile_into_domain_lists() -> Result<()> {
    let policy_src = r#"
network_rule(host = "google.com", protocol = "http", decision = "allow")
network_rule(host = "api.github.com", protocol = "https", decision = "allow")
network_rule(host = "blocked.example.com", protocol = "https", decision = "deny")
network_rule(host = "prompt-only.example.com", protocol = "https", decision = "prompt")
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("network.rules", policy_src)?;
    let policy = parser.build();

    assert_eq!(policy.network_rules().len(), 4);
    assert_eq!(
        policy.network_rules()[1].protocol,
        NetworkRuleProtocol::Https
    );

    let (allowed, denied) = policy.compiled_network_domains();
    assert_eq!(
        allowed,
        vec!["google.com".to_string(), "api.github.com".to_string()]
    );
    assert_eq!(denied, vec!["blocked.example.com".to_string()]);
    Ok(())
}

#[test]
fn network_rule_rejects_wildcard_hosts() {
    let mut parser = PolicyParser::new();
    let err = parser
        .parse(
            "network.rules",
            r#"network_rule(host="*", protocol="http", decision="allow")"#,
        )
        .expect_err("wildcard network_rule host should fail");
    assert!(err.to_string().contains("wildcards are not allowed"));
}

#[test]
fn basic_match() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["git", "status"],
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();
    let cmd = tokens(&["git", "status"]);
    let evaluation = policy.check(&cmd, &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["git", "status"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        evaluation
    );
    Ok(())
}

#[test]
fn justification_is_attached_to_forbidden_matches() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["rm"],
    decision = "forbidden",
    justification = "destructive command",
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let evaluation = policy.check(
        &tokens(&["rm", "-rf", "/some/important/folder"]),
        &allow_all,
    );
    assert_eq!(
        Evaluation {
            decision: Decision::Forbidden,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["rm"]),
                decision: Decision::Forbidden,
                resolved_program: None,
                justification: Some("destructive command".to_string()),
            }],
        },
        evaluation
    );
    Ok(())
}

#[test]
fn justification_can_be_used_with_allow_decision() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["ls"],
    decision = "allow",
    justification = "safe and commonly used",
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let evaluation = policy.check(&tokens(&["ls", "-l"]), &prompt_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["ls"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: Some("safe and commonly used".to_string()),
            }],
        },
        evaluation
    );
    Ok(())
}

#[test]
fn justification_cannot_be_empty() {
    let policy_src = r#"
prefix_rule(
    pattern = ["ls"],
    decision = "prompt",
    justification = "   ",
)
    "#;
    let mut parser = PolicyParser::new();
    let err = parser
        .parse("test.rules", policy_src)
        .expect_err("expected parse error");
    assert!(
        err.to_string()
            .contains("invalid rule: justification cannot be empty")
    );
}

#[test]
fn add_prefix_rule_extends_policy() -> Result<()> {
    let mut policy = Policy::empty();
    policy.add_prefix_rule(&tokens(&["ls", "-l"]), Decision::Prompt)?;

    let rules = rule_snapshots(policy.rules().get_vec("ls").context("missing ls rules")?);
    assert_eq!(
        vec![PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from("ls"),
                rest: vec![PatternToken::Single(String::from("-l"))].into(),
            },
            decision: Decision::Prompt,
            justification: None,
        }],
        rules
    );

    let evaluation = policy.check(&tokens(&["ls", "-l", "/some/important/folder"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Prompt,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["ls", "-l"]),
                decision: Decision::Prompt,
                resolved_program: None,
                justification: None,
            }],
        },
        evaluation
    );
    Ok(())
}

#[test]
fn add_prefix_rule_rejects_empty_prefix() -> Result<()> {
    let mut policy = Policy::empty();
    let result = policy.add_prefix_rule(&[], Decision::Allow);

    match result.unwrap_err() {
        Error::InvalidPattern(message) => assert_eq!(message, "prefix cannot be empty"),
        other => panic!("expected InvalidPattern(..), got {other:?}"),
    }
    Ok(())
}

#[test]
fn add_prefix_rule_rejects_empty_tokens() {
    let mut policy = Policy::empty();

    for prefix in [tokens(&[""]), tokens(&[" ", "status"])] {
        assert!(matches!(
            policy.add_prefix_rule(&prefix, Decision::Allow),
            Err(Error::InvalidPattern(message)) if message == "token cannot be empty"
        ));
    }
}

#[test]
fn starlark_rules_reject_empty_pattern_tokens() {
    for source in [
        r#"prefix_rule(pattern = [""], decision = "allow")"#,
        r#"prefix_rule(pattern = [["git", ""]], decision = "allow")"#,
    ] {
        let mut parser = PolicyParser::new();
        let err = parser
            .parse("empty.rules", source)
            .expect_err("empty semantic tokens must be rejected");
        assert!(err.to_string().contains("token cannot be empty"), "{err:#}");
    }
}

#[test]
fn parses_multiple_policy_files() -> Result<()> {
    let first_policy = r#"
prefix_rule(
    pattern = ["git"],
    decision = "prompt",
)
    "#;
    let second_policy = r#"
prefix_rule(
    pattern = ["git", "commit"],
    decision = "forbidden",
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("first.rules", first_policy)?;
    parser.parse("second.rules", second_policy)?;
    let policy = parser.build();

    let git_rules = rule_snapshots(policy.rules().get_vec("git").context("missing git rules")?);
    assert_eq!(
        vec![
            PrefixRule {
                pattern: PrefixPattern {
                    first: Arc::from("git"),
                    rest: Vec::<PatternToken>::new().into(),
                },
                decision: Decision::Prompt,
                justification: None,
            },
            PrefixRule {
                pattern: PrefixPattern {
                    first: Arc::from("git"),
                    rest: vec![PatternToken::Single("commit".to_string())].into(),
                },
                decision: Decision::Forbidden,
                justification: None,
            },
        ],
        git_rules
    );

    let status_eval = policy.check(&tokens(&["git", "status"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Prompt,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["git"]),
                decision: Decision::Prompt,
                resolved_program: None,
                justification: None,
            }],
        },
        status_eval
    );

    let commit_eval = policy.check(&tokens(&["git", "commit", "-m", "hi"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Forbidden,
            matched_rules: vec![
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git"]),
                    decision: Decision::Prompt,
                    resolved_program: None,
                    justification: None,
                },
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git", "commit"]),
                    decision: Decision::Forbidden,
                    resolved_program: None,
                    justification: None,
                },
            ],
        },
        commit_eval
    );
    Ok(())
}

#[test]
fn only_first_token_alias_expands_to_multiple_rules() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = [["bash", "sh"], ["-c", "-l"]],
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let bash_rules = rule_snapshots(
        policy
            .rules()
            .get_vec("bash")
            .context("missing bash rules")?,
    );
    let sh_rules = rule_snapshots(policy.rules().get_vec("sh").context("missing sh rules")?);
    assert_eq!(
        vec![PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from("bash"),
                rest: vec![PatternToken::Alts(vec!["-c".to_string(), "-l".to_string()])].into(),
            },
            decision: Decision::Allow,
            justification: None,
        }],
        bash_rules
    );
    assert_eq!(
        vec![PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from("sh"),
                rest: vec![PatternToken::Alts(vec!["-c".to_string(), "-l".to_string()])].into(),
            },
            decision: Decision::Allow,
            justification: None,
        }],
        sh_rules
    );

    let bash_eval = policy.check(&tokens(&["bash", "-c", "echo", "hi"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["bash", "-c"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        bash_eval
    );

    let sh_eval = policy.check(&tokens(&["sh", "-l", "echo", "hi"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["sh", "-l"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        sh_eval
    );
    Ok(())
}

#[test]
fn tail_aliases_are_not_cartesian_expanded() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["npm", ["i", "install"], ["--legacy-peer-deps", "--no-save"]],
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let rules = rule_snapshots(policy.rules().get_vec("npm").context("missing npm rules")?);
    assert_eq!(
        vec![PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from("npm"),
                rest: vec![
                    PatternToken::Alts(vec!["i".to_string(), "install".to_string()]),
                    PatternToken::Alts(vec![
                        "--legacy-peer-deps".to_string(),
                        "--no-save".to_string(),
                    ]),
                ]
                .into(),
            },
            decision: Decision::Allow,
            justification: None,
        }],
        rules
    );

    let npm_i = policy.check(&tokens(&["npm", "i", "--legacy-peer-deps"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["npm", "i", "--legacy-peer-deps"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        npm_i
    );

    let npm_install = policy.check(
        &tokens(&["npm", "install", "--no-save", "leftpad"]),
        &allow_all,
    );
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["npm", "install", "--no-save"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        npm_install
    );
    Ok(())
}

#[test]
fn match_and_not_match_examples_are_enforced() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["git", "status"],
    match = [["git", "status"], "git status"],
    not_match = [
        ["git", "--config", "color.status=always", "status"],
        "git --config color.status=always status",
    ],
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();
    let match_eval = policy.check(&tokens(&["git", "status"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["git", "status"]),
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        },
        match_eval
    );

    let no_match_eval = policy.check(
        &tokens(&["git", "--config", "color.status=always", "status"]),
        &allow_all,
    );
    assert_eq!(
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::HeuristicsRuleMatch {
                command: tokens(&["git", "--config", "color.status=always", "status",]),
                decision: Decision::Allow,
            }],
        },
        no_match_eval
    );
    Ok(())
}

#[test]
fn strictest_decision_wins_across_matches() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["git"],
    decision = "prompt",
)
prefix_rule(
    pattern = ["git", "commit"],
    decision = "forbidden",
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let commit = policy.check(&tokens(&["git", "commit", "-m", "hi"]), &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Forbidden,
            matched_rules: vec![
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git"]),
                    decision: Decision::Prompt,
                    resolved_program: None,
                    justification: None,
                },
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git", "commit"]),
                    decision: Decision::Forbidden,
                    resolved_program: None,
                    justification: None,
                },
            ],
        },
        commit
    );
    Ok(())
}

#[test]
fn strictest_decision_across_multiple_commands() -> Result<()> {
    let policy_src = r#"
prefix_rule(
    pattern = ["git"],
    decision = "prompt",
)
prefix_rule(
    pattern = ["git", "commit"],
    decision = "forbidden",
)
    "#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();

    let commands = vec![
        tokens(&["git", "status"]),
        tokens(&["git", "commit", "-m", "hi"]),
    ];

    let evaluation = policy.check_multiple(&commands, &allow_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Forbidden,
            matched_rules: vec![
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git"]),
                    decision: Decision::Prompt,
                    resolved_program: None,
                    justification: None,
                },
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git"]),
                    decision: Decision::Prompt,
                    resolved_program: None,
                    justification: None,
                },
                RuleMatch::PrefixRuleMatch {
                    matched_prefix: tokens(&["git", "commit"]),
                    decision: Decision::Forbidden,
                    resolved_program: None,
                    justification: None,
                },
            ],
        },
        evaluation
    );
    Ok(())
}

#[test]
fn heuristics_match_is_returned_when_no_policy_matches() {
    let policy = Policy::empty();
    let command = tokens(&["python"]);

    let evaluation = policy.check(&command, &prompt_all);
    assert_eq!(
        Evaluation {
            decision: Decision::Prompt,
            matched_rules: vec![RuleMatch::HeuristicsRuleMatch {
                command,
                decision: Decision::Prompt,
            }],
        },
        evaluation
    );
}

#[test]
fn parses_host_executable_paths() -> Result<()> {
    let homebrew_git = host_absolute_path(&["opt", "homebrew", "bin", "git"]);
    let usr_git = host_absolute_path(&["usr", "bin", "git"]);
    let homebrew_git_literal = starlark_string(&homebrew_git);
    let usr_git_literal = starlark_string(&usr_git);
    let policy_src = format!(
        r#"
host_executable(
    name = "git",
    paths = [
        "{homebrew_git_literal}",
        "{usr_git_literal}",
        "{usr_git_literal}",
    ],
)
"#
    );
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &policy_src)?;
    let policy = parser.build();

    assert_eq!(
        policy
            .host_executables()
            .get("git")
            .expect("missing git host executable")
            .as_ref(),
        [absolute_path(&homebrew_git), absolute_path(&usr_git)]
    );
    Ok(())
}

#[test]
fn host_executable_rejects_non_absolute_path() {
    let policy_src = r#"
host_executable(name = "git", paths = ["git"])
"#;
    let mut parser = PolicyParser::new();
    let err = parser
        .parse("test.rules", policy_src)
        .expect_err("expected parse error");
    assert!(
        err.to_string()
            .contains("host_executable paths must be absolute")
    );
}

#[test]
fn host_executable_rejects_name_with_path_separator() {
    let git_path = host_absolute_path(&["usr", "bin", "git"]);
    let git_path_literal = starlark_string(&git_path);
    let policy_src =
        format!(r#"host_executable(name = "{git_path_literal}", paths = ["{git_path_literal}"])"#);
    let mut parser = PolicyParser::new();
    let err = parser
        .parse("test.rules", &policy_src)
        .expect_err("expected parse error");
    assert!(
        err.to_string()
            .contains("host_executable name must be a bare executable name")
    );
}

#[test]
fn host_executable_rejects_path_with_wrong_basename() {
    let rg_path = host_absolute_path(&["usr", "bin", "rg"]);
    let rg_path_literal = starlark_string(&rg_path);
    let policy_src = format!(r#"host_executable(name = "git", paths = ["{rg_path_literal}"])"#);
    let mut parser = PolicyParser::new();
    let err = parser
        .parse("test.rules", &policy_src)
        .expect_err("expected parse error");
    assert!(err.to_string().contains("must have basename `git`"));
}

#[test]
fn host_executable_last_definition_wins() -> Result<()> {
    let usr_git = host_absolute_path(&["usr", "bin", "git"]);
    let homebrew_git = host_absolute_path(&["opt", "homebrew", "bin", "git"]);
    let usr_git_literal = starlark_string(&usr_git);
    let homebrew_git_literal = starlark_string(&homebrew_git);
    let mut parser = PolicyParser::new();
    parser.parse(
        "shared.rules",
        &format!(r#"host_executable(name = "git", paths = ["{usr_git_literal}"])"#),
    )?;
    parser.parse(
        "user.rules",
        &format!(r#"host_executable(name = "git", paths = ["{homebrew_git_literal}"])"#),
    )?;
    let policy = parser.build();

    assert_eq!(
        policy
            .host_executables()
            .get("git")
            .expect("missing git host executable")
            .as_ref(),
        [absolute_path(&homebrew_git)]
    );
    Ok(())
}

#[test]
fn host_executable_resolution_uses_basename_rule_when_allowed() -> Result<()> {
    let git_name = host_executable_name("git");
    let git_path = host_absolute_path(&["usr", "bin", &git_name]);
    let git_path_literal = starlark_string(&git_path);
    let policy_src = format!(
        r#"
prefix_rule(pattern = ["git", "status"], decision = "prompt")
host_executable(name = "git", paths = ["{git_path_literal}"])
"#
    );
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &policy_src)?;
    let policy = parser.build();

    let evaluation = policy.check_with_options(
        &[git_path.clone(), "status".to_string()],
        &allow_all,
        &MatchOptions {
            resolve_host_executables: true,
        },
    );
    assert_eq!(
        evaluation,
        Evaluation {
            decision: Decision::Prompt,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["git", "status"]),
                decision: Decision::Prompt,
                resolved_program: Some(absolute_path(&git_path)),
                justification: None,
            }],
        }
    );
    Ok(())
}

#[test]
fn prefix_rule_examples_honor_host_executable_resolution() -> Result<()> {
    let allowed_git_name = host_executable_name("git");
    let allowed_git = host_absolute_path(&["usr", "bin", &allowed_git_name]);
    let other_git = host_absolute_path(&["opt", "homebrew", "bin", &allowed_git_name]);
    let allowed_git_literal = starlark_string(&allowed_git);
    let other_git_literal = starlark_string(&other_git);
    let policy_src = format!(
        r#"
prefix_rule(
    pattern = ["git", "status"],
    match = [["{allowed_git_literal}", "status"]],
    not_match = [["{other_git_literal}", "status"]],
)
host_executable(name = "git", paths = ["{allowed_git_literal}"])
"#
    );

    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &policy_src)?;

    let policy = parser.build();
    let options = MatchOptions {
        resolve_host_executables: true,
    };
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&allowed_git, "status"]), &prompt_all, &options)
            .decision,
        Decision::Allow
    );
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&other_git, "status"]), &prompt_all, &options)
            .decision,
        Decision::Prompt
    );

    Ok(())
}

#[test]
fn host_executable_resolution_respects_explicit_empty_allowlist() -> Result<()> {
    let policy_src = r#"
prefix_rule(pattern = ["git"], decision = "prompt")
host_executable(name = "git", paths = [])
"#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();
    let git_path = host_absolute_path(&["usr", "bin", "git"]);

    let evaluation = policy.check_with_options(
        &[git_path.clone(), "status".to_string()],
        &allow_all,
        &MatchOptions {
            resolve_host_executables: true,
        },
    );
    assert_eq!(
        evaluation,
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::HeuristicsRuleMatch {
                command: vec![git_path, "status".to_string()],
                decision: Decision::Allow,
            }],
        }
    );
    Ok(())
}

#[test]
fn host_executable_resolution_ignores_path_not_in_allowlist() -> Result<()> {
    let allowed_git = host_absolute_path(&["usr", "bin", "git"]);
    let other_git = host_absolute_path(&["opt", "homebrew", "bin", "git"]);
    let allowed_git_literal = starlark_string(&allowed_git);
    let policy_src = format!(
        r#"
prefix_rule(pattern = ["git"], decision = "prompt")
host_executable(name = "git", paths = ["{allowed_git_literal}"])
"#
    );
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &policy_src)?;
    let policy = parser.build();

    let evaluation = policy.check_with_options(
        &[other_git.clone(), "status".to_string()],
        &allow_all,
        &MatchOptions {
            resolve_host_executables: true,
        },
    );
    assert_eq!(
        evaluation,
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::HeuristicsRuleMatch {
                command: vec![other_git, "status".to_string()],
                decision: Decision::Allow,
            }],
        }
    );
    Ok(())
}

#[test]
fn host_executable_resolution_falls_back_without_mapping() -> Result<()> {
    let policy_src = r#"
prefix_rule(pattern = ["git"], decision = "prompt")
"#;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", policy_src)?;
    let policy = parser.build();
    let git_path = host_absolute_path(&["usr", "bin", "git"]);

    let evaluation = policy.check_with_options(
        &[git_path.clone(), "status".to_string()],
        &allow_all,
        &MatchOptions {
            resolve_host_executables: true,
        },
    );
    assert_eq!(
        evaluation,
        Evaluation {
            decision: Decision::Prompt,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: tokens(&["git"]),
                decision: Decision::Prompt,
                resolved_program: Some(absolute_path(&git_path)),
                justification: None,
            }],
        }
    );
    Ok(())
}

#[test]
fn host_executable_resolution_does_not_override_exact_match() -> Result<()> {
    let git_path = host_absolute_path(&["usr", "bin", "git"]);
    let git_path_literal = starlark_string(&git_path);
    let policy_src = format!(
        r#"
prefix_rule(pattern = ["{git_path_literal}"], decision = "allow")
prefix_rule(pattern = ["git"], decision = "prompt")
host_executable(name = "git", paths = ["{git_path_literal}"])
"#
    );
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &policy_src)?;
    let policy = parser.build();

    let evaluation = policy.check_with_options(
        &[git_path.clone(), "status".to_string()],
        &allow_all,
        &MatchOptions {
            resolve_host_executables: true,
        },
    );
    assert_eq!(
        evaluation,
        Evaluation {
            decision: Decision::Allow,
            matched_rules: vec![RuleMatch::PrefixRuleMatch {
                matched_prefix: vec![git_path],
                decision: Decision::Allow,
                resolved_program: None,
                justification: None,
            }],
        }
    );
    Ok(())
}

#[test]
fn literal_blank_arguments_round_trip_and_match_exactly() -> Result<()> {
    for argument in ["", " ", "\t"] {
        let command = tokens(&["printf", "%s", argument]);
        let mut policy = Policy::empty();
        policy.add_prefix_rule(&command, Decision::Allow)?;
        assert_eq!(
            policy.check(&command, &prompt_all).decision,
            Decision::Allow
        );
        assert_eq!(
            policy
                .check(&tokens(&["printf", "%s", "other"]), &prompt_all)
                .decision,
            Decision::Prompt
        );
        let tmp = tempdir()?;
        let path = tmp.path().join("test.rules");
        blocking_append_allow_prefix_rule(&path, &command)?;
        let mut parser = PolicyParser::new();
        parser.parse("test.rules", &fs::read_to_string(path)?)?;
        assert_eq!(
            parser.build().check(&command, &prompt_all).decision,
            Decision::Allow
        );
    }
    let mut parser = PolicyParser::new();
    parser.parse(
        "alternatives.rules",
        r#"prefix_rule(pattern=["printf", ["", " ", "\t"]], decision="allow")"#,
    )?;
    let policy = parser.build();
    for argument in ["", " ", "\t"] {
        assert_eq!(
            policy
                .check(&tokens(&["printf", argument]), &prompt_all)
                .decision,
            Decision::Allow
        );
    }
    assert_eq!(
        policy
            .check(&tokens(&["printf", "other"]), &prompt_all)
            .decision,
        Decision::Prompt
    );
    Ok(())
}

#[test]
fn amendment_does_not_confuse_string_contents_with_declaration() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join("test.rules");
    fs::write(
        &path,
        "unused = \"\"\"\nprefix_rule(pattern=[\"git\", \"status\"], decision=\"allow\")\n\"\"\"\n",
    )?;
    blocking_append_allow_prefix_rule(&path, &tokens(&["git", "status"]))?;
    let contents = fs::read_to_string(&path)?;
    let mut parser = PolicyParser::new();
    parser.parse("test.rules", &contents)?;
    assert_eq!(
        parser
            .build()
            .check(&tokens(&["git", "status"]), &prompt_all)
            .decision,
        Decision::Allow
    );
    blocking_append_allow_prefix_rule(&path, &tokens(&["git", "status"]))?;
    assert_eq!(fs::read_to_string(path)?, contents);
    Ok(())
}

#[cfg(windows)]
#[test]
fn suffixed_windows_rules_resolve_without_authorizing_other_suffixes() -> Result<()> {
    let exe = host_absolute_path(&["trusted", "git.exe"]);
    let cmd = host_absolute_path(&["trusted", "git.cmd"]);
    let mut policy = Policy::empty();
    policy.add_prefix_rule(&tokens(&["Git.EXE", "status"]), Decision::Allow)?;
    policy.add_prefix_rule(&tokens(&["git.cmd", "status"]), Decision::Forbidden)?;
    policy.set_host_executable_paths("git".into(), vec![absolute_path(&exe), absolute_path(&cmd)]);
    let options = MatchOptions {
        resolve_host_executables: true,
    };
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&exe, "status"]), &prompt_all, &options)
            .decision,
        Decision::Allow
    );
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&cmd, "status"]), &prompt_all, &options)
            .decision,
        Decision::Forbidden
    );
    let untrusted = host_absolute_path(&["untrusted", "git.exe"]);
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&untrusted, "status"]), &prompt_all, &options)
            .decision,
        Decision::Prompt
    );
    policy.add_prefix_rule(&tokens(&["git", "status"]), Decision::Forbidden)?;
    assert_eq!(
        policy
            .check_with_options(&tokens(&[&exe, "status"]), &prompt_all, &options)
            .decision,
        Decision::Forbidden
    );
    Ok(())
}
