use super::*;
use pretty_assertions::assert_eq;

#[test]
fn explicit_plugin_instructions_use_namespace_for_skill_prefix() {
    let rendered = render_explicit_plugin_instructions(
        &PluginCapabilitySummary {
            config_name: "github@test".to_string(),
            display_name: "GitHub".to_string(),
            has_skills: true,
            ..PluginCapabilitySummary::default()
        },
        &[],
        &[],
    )
    .expect("plugin instructions should render");

    let expected = "Capabilities from the `GitHub` plugin:\n\
- Skills from this plugin are prefixed with `github:`.\n\
Use these plugin-associated capabilities to help solve the task.";

    assert_eq!(rendered, expected);
}
