use super::*;

fn parse(args: &[&str]) -> Result<MultitoolCli, clap::Error> {
    try_parse_multitool_cli(args.iter().map(|arg| std::ffi::OsString::from(*arg)))
}

#[test]
fn config_and_features_keep_every_occurrence_in_argv_order() {
    for command in [
        vec!["exec", "hello"],
        vec!["mcp", "list"],
        vec!["plugin", "list"],
        vec!["features", "list"],
        vec!["debug", "prompt-input", "hello"],
    ] {
        let mut args = vec![
            "codex",
            "--enable",
            "plugins",
            "-c",
            "features.plugins=false",
        ];
        args.extend(command);
        args.extend([
            "--disable",
            "shell_tool",
            "--disable",
            "plugins",
            "--enable",
            "plugins",
        ]);
        let cli = parse(&args).expect("parse every mode");
        assert_eq!(
            cli.config_overrides.raw_overrides,
            [
                "features.plugins=true",
                "features.plugins=false",
                "features.shell_tool=false",
                "features.plugins=false",
                "features.plugins=true",
            ]
        );
        assert!(cli.feature_toggles.enable.is_empty());
        assert!(cli.feature_toggles.disable.is_empty());
        match cli.subcommand.unwrap() {
            Subcommand::Exec(exec) => assert!(exec.config_overrides.raw_overrides.is_empty()),
            Subcommand::Mcp(mcp) => assert!(mcp.config_overrides.raw_overrides.is_empty()),
            Subcommand::Plugin(plugin) => assert!(plugin.config_overrides.raw_overrides.is_empty()),
            _ => {}
        }
    }
    let cli = parse(&[
        "codex",
        "--disable",
        "plugins",
        "mcp",
        "--enable",
        "shell_tool",
        "list",
        "--enable",
        "plugins",
    ])
    .unwrap();
    assert_eq!(
        cli.config_overrides.raw_overrides,
        [
            "features.plugins=false",
            "features.shell_tool=true",
            "features.plugins=true"
        ]
    );
}

#[test]
fn invalid_ancestor_feature_is_not_hidden_by_a_valid_child_flag() {
    let error = parse(&[
        "codex",
        "--disable",
        "does_not_exist",
        "features",
        "list",
        "--disable",
        "shell_tool",
    ])
    .unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert!(
        error
            .to_string()
            .contains("Unknown feature flag: does_not_exist")
    );
}

#[test]
fn delimited_prompt_is_not_lowered_as_a_feature_flag() {
    let cli = parse(&["codex", "exec", "--", "--disable plugins"]).unwrap();
    assert!(cli.config_overrides.raw_overrides.is_empty());
    let Some(Subcommand::Exec(exec)) = cli.subcommand else {
        panic!("exec")
    };
    assert_eq!(exec.prompt.as_deref(), Some("--disable plugins"));
}

#[test]
fn attached_config_flags_are_lowered_without_consuming_delimited_prompt() {
    let cli = parse(&[
        "codex",
        "--enable=plugins",
        "exec",
        "-cfeatures.plugins=false",
        "--",
        "--config=literal prompt",
    ])
    .unwrap();
    assert_eq!(
        cli.config_overrides.raw_overrides,
        ["features.plugins=true", "features.plugins=false"]
    );
    assert!(cli.feature_toggles.enable.is_empty());
    let Some(Subcommand::Exec(exec)) = cli.subcommand else {
        panic!("exec")
    };
    assert!(exec.config_overrides.raw_overrides.is_empty());
    assert_eq!(exec.prompt.as_deref(), Some("--config=literal prompt"));
}

#[test]
fn images_do_not_consume_mode_or_prompt() {
    for mode in ["exec", "e"] {
        let cli = parse(&[
            "codex",
            "-i",
            "a b.png,猫.png",
            "-i",
            "./exec",
            mode,
            "describe",
        ])
        .unwrap();
        assert_eq!(
            cli.root.images,
            [
                PathBuf::from("a b.png"),
                PathBuf::from("猫.png"),
                PathBuf::from("./exec")
            ]
        );
        let Some(Subcommand::Exec(mut exec)) = cli.subcommand else {
            panic!("exec")
        };
        inherit_exec_root_options(&mut exec, &cli.root).unwrap();
        assert_eq!(exec.prompt.as_deref(), Some("describe"));
        assert_eq!(exec.shared.images, cli.root.images);
    }
    assert!(parse(&["codex", "-i", "image.png", "describe"]).is_err());
    assert!(parse(&["codex", "-i"]).is_err());
    let cli = parse(&[
        "codex",
        "debug",
        "prompt-input",
        "-i",
        "image.png",
        "describe",
    ])
    .unwrap();
    let Some(Subcommand::Debug(DebugCommand {
        subcommand: DebugSubcommand::PromptInput(cmd),
    })) = cli.subcommand
    else {
        panic!("prompt-input")
    };
    assert_eq!(cmd.prompt.as_deref(), Some("describe"));
}

#[test]
fn sandbox_delimiter_selects_child_not_provisioning() {
    let cli = parse(&[
        "codex",
        "sandbox",
        "--",
        "setup",
        "--elevated",
        "--current-user",
    ])
    .unwrap();
    let Some(Subcommand::Sandbox(sandbox)) = cli.subcommand else {
        panic!("sandbox")
    };
    assert!(sandbox.action.is_none());
    assert_eq!(
        sandbox.execution.command,
        ["setup", "--elevated", "--current-user"]
    );
    let cli = parse(&["codex", "sandbox", "setup", "--elevated", "--current-user"]).unwrap();
    let Some(Subcommand::Sandbox(sandbox)) = cli.subcommand else {
        panic!("sandbox")
    };
    assert!(matches!(sandbox.action, Some(SandboxAction::Setup(_))));
    assert!(sandbox.execution.command.is_empty());
    let cli = parse(&["codex", "sandbox", "python", "setup"]).unwrap();
    let Some(Subcommand::Sandbox(sandbox)) = cli.subcommand else {
        panic!("sandbox")
    };
    assert!(sandbox.action.is_none());
    assert_eq!(sandbox.execution.command, ["python", "setup"]);
    let cli = parse(&["codex", "sandbox", "--profile", "work", "--", "setup"]).unwrap();
    let Some(Subcommand::Sandbox(sandbox)) = cli.subcommand else {
        panic!("sandbox")
    };
    assert!(sandbox.action.is_none());
    assert_eq!(sandbox.execution.command, ["setup"]);
    let error = parse(&["codex", "sandbox", "setup", "--help"]).unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    assert_eq!(error.exit_code(), 0);
    let error = parse(&[
        "codex",
        "sandbox",
        "--profile",
        "work",
        "setup",
        "--elevated",
        "--current-user",
    ])
    .unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn root_search_reaches_exec_and_approval_is_rejected() {
    for mode in ["exec", "e"] {
        let cli = parse(&[
            "codex",
            "--search",
            "-c",
            "web_search=\"disabled\"",
            mode,
            "research",
        ])
        .unwrap();
        let Some(Subcommand::Exec(mut exec)) = cli.subcommand else {
            panic!("exec")
        };
        inherit_exec_root_options(&mut exec, &cli.root).unwrap();
        prepend_config_flags(&mut exec.config_overrides, cli.config_overrides);
        let overrides = exec.config_overrides.parse_overrides().unwrap();
        assert_eq!(
            overrides.last(),
            Some(&("web_search".into(), toml::Value::String("live".into())))
        );
    }
    let cli = parse(&["codex", "--ask-for-approval", "never", "exec", "hello"]).unwrap();
    let Some(Subcommand::Exec(mut exec)) = cli.subcommand else {
        panic!("exec")
    };
    assert!(inherit_exec_root_options(&mut exec, &cli.root).is_err());
}

#[test]
fn debug_app_server_test_client_is_not_shipped() {
    assert!(
        parse(&["codex", "debug", "app-server", "send-message-v2", "probe"]).is_err(),
        "the removed test-client command must not be available"
    );
}

#[test]
fn config_flags_do_not_strip_trailing_child_command_arguments() {
    for (args, expected_overrides) in [
        (
            vec![
                "codex", "-c", "model=x", "sandbox", "bash", "-c", "echo hi", "--enable", "x",
            ],
            "model=x",
        ),
        (
            vec![
                "codex",
                "sandbox",
                "--disable",
                "plugins",
                "bash",
                "-c",
                "echo hi",
                "--enable",
                "x",
            ],
            "features.plugins=false",
        ),
    ] {
        let cli = parse(&args).unwrap();
        assert_eq!(
            cli.config_overrides.raw_overrides,
            [expected_overrides],
            "{args:?}"
        );
        let Some(Subcommand::Sandbox(sandbox)) = cli.subcommand else {
            panic!("sandbox")
        };
        assert_eq!(
            sandbox.execution.command,
            ["bash", "-c", "echo hi", "--enable", "x"],
            "{args:?}"
        );
    }
}
