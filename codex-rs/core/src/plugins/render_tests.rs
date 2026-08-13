use super::*;
use pretty_assertions::assert_eq;

#[test]
fn render_plugins_section_returns_none_for_empty_plugins() {
    assert_eq!(render_plugins_section(&[]), None);
}

#[test]
fn render_plugins_section_keeps_plugin_usage_guidance_without_listing_plugins() {
    let rendered = render_plugins_section(&[PluginCapabilitySummary {
        config_name: "sample@test".to_string(),
        display_name: "sample".to_string(),
        description: Some("inspect sample data".to_string()),
        has_skills: true,
        ..PluginCapabilitySummary::default()
    }])
    .expect("plugin section should render");

    let expected = "<plugins_instructions>\n## Plugins\nPlugins contribute skills (`plugin_name:skill`), MCP tools, or apps; invoke the contributed capability, not the bundle. Prefer a named plugin's relevant capabilities. If none are callable, say so briefly and use the best fallback.\n</plugins_instructions>";

    assert_eq!(rendered, expected);
}

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
