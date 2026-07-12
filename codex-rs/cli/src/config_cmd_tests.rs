use super::*;

#[test]
fn renders_filtered_config_explanation() {
    let rendered = render_explain(Some("sandbox"));

    assert!(rendered.contains("sandbox_mode"));
    assert!(rendered.contains("sandbox_workspace_write"));
    assert!(!rendered.contains("- model: Default model used for new turns."));
}

#[test]
fn subcommand_name_is_specific() {
    let command = ConfigCli {
        subcommand: ConfigSubcommand::Explain(ConfigExplainArgs { filter: None }),
    };

    assert_eq!(command.config_subcommand_name(), "config explain");
}
