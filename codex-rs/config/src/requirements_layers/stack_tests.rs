use super::super::RequirementsLayerEntry;
use super::super::hooks::HookDirectoryField;
use super::RequirementsCompositionError;
use super::compose_requirements_for_hostname;
use super::compose_requirements_for_hostname_and_hook_directory;
use super::compose_requirements_with_hostname_resolver;
use crate::ConfigRequirementsToml;
use crate::ConfigRequirementsWithSources;
use crate::RequirementSource;
use crate::Sourced;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::cell::Cell;
use std::collections::BTreeMap;
use tempfile::TempDir;

fn layer(id: &str, name: &str, contents: &str) -> RequirementsLayerEntry {
    RequirementsLayerEntry::from_toml(
        RequirementSource::EnterpriseManaged {
            id: id.to_string(),
            name: name.to_string(),
        },
        contents,
    )
}

fn compose(
    layers: Vec<RequirementsLayerEntry>,
) -> Result<Option<ConfigRequirementsToml>, RequirementsCompositionError> {
    Ok(
        compose_requirements_for_hostname(layers, /*hostname*/ None)?
            .map(ConfigRequirementsWithSources::into_toml),
    )
}

fn compose_with_hook_directory_field(
    layers: Vec<RequirementsLayerEntry>,
    hook_directory_field: HookDirectoryField,
) -> Result<Option<ConfigRequirementsToml>, RequirementsCompositionError> {
    Ok(compose_requirements_for_hostname_and_hook_directory(
        layers,
        /*hostname*/ None,
        hook_directory_field,
    )?
    .map(ConfigRequirementsWithSources::into_toml))
}

fn expected_requirements(contents: impl AsRef<str>) -> ConfigRequirementsToml {
    toml::from_str(contents.as_ref()).expect("parse expected requirements TOML")
}

#[test]
fn empty_layers_compose_to_none() {
    let composed = compose(Vec::new()).expect("compose empty layers");
    assert_eq!(composed, None);
}

#[test]
fn composition_preserves_semantic_emptiness() {
    for (contents, empty, composed_empty) in [
        ("", true, true),
        ("guardian_policy_config = '   '", true, true),
        ("[features]", true, true),
        ("[hooks]", true, true),
        ("[models.new_thread]", true, true),
        ("[computer_use]", true, true),
        ("[windows]", true, true),
        ("[plugins.empty]", true, true),
        ("[marketplaces]", true, true),
        ("[apps]", true, true),
        ("[rules]\nprefix_rules = []", false, false),
        ("allowed_approval_policies = []", false, false),
        ("allow_remote_control = false", false, false),
        ("default_permissions = ''", false, false),
        ("guardian_policy_config = 'policy'", false, false),
        ("[models.new_thread]\nmodel = 'model'", false, false),
        ("[mcp_servers]", false, false),
        // Special-field stripping prunes an empty permissions table.
        ("[permissions]", false, true),
        ("[experimental_network]", false, false),
    ] {
        let requirements = expected_requirements(contents);
        assert_eq!(requirements.is_empty(), empty, "{contents}");
        let mut sourced = ConfigRequirementsWithSources::default();
        sourced.merge_unset_fields(RequirementSource::Unknown, requirements);
        assert_eq!(sourced.is_empty(), empty, "{contents}");
        assert_eq!(sourced.clone().into_toml().is_empty(), empty, "{contents}");
        assert_eq!(
            compose(vec![layer("req", "Layer", contents)])
                .expect("compose requirements")
                .is_none(),
            composed_empty,
            "{contents}"
        );
    }
}

#[test]
fn text_and_value_layers_preserve_context_and_results() {
    let base = TempDir::new().expect("create base directory");
    let base = AbsolutePathBuf::try_from(base.path()).expect("absolute base");
    let contents = r#"
[feature_requirements]
enabled = true

[permissions.filesystem]
deny_read = ["./private"]

[marketplaces.allowed_sources.local]
source = "local"
path = "../plugins"
"#;
    let source = layer("req", "Layer", "").source;
    for entry in [
        RequirementsLayerEntry::from_toml(source.clone(), contents),
        RequirementsLayerEntry::from_toml_value(
            source.clone(),
            toml::from_str(contents).expect("parse value"),
        ),
    ] {
        let output = super::compose_requirements([entry.with_base_dir(base.clone())])
            .expect("compose layer")
            .expect("requirements present");
        assert_eq!(
            output.permissions.as_ref().expect("permissions").source,
            source
        );
        assert_eq!(
            output.into_toml(),
            expected_requirements(format!(
                r#"
[features]
enabled = true

[permissions.filesystem]
deny_read = [{:?}]

[marketplaces.allowed_sources.local]
source = "local"
path = "../plugins"
"#,
                base.as_path().join("private").to_string_lossy()
            ))
        );
    }
}

#[test]
fn invalid_lower_layer_cannot_be_hidden_for_either_input_representation() {
    let source = layer("bad", "Bad layer", "").source;
    let contents = "allowed_approval_policies = [1]";
    for entry in [
        RequirementsLayerEntry::from_toml(source.clone(), contents),
        RequirementsLayerEntry::from_toml_value(
            source.clone(),
            toml::from_str(contents).expect("parse syntactically valid TOML"),
        ),
    ] {
        let err = super::compose_requirements([
            entry,
            layer("high", "High", "allowed_approval_policies = ['never']"),
        ])
        .expect_err("validate every layer before merging");
        let RequirementsCompositionError::Parse {
            layer_source,
            message,
        } = err
        else {
            panic!("expected layer parse error: {err}");
        };
        assert_eq!(layer_source, source);
        assert!(message.contains("allowed_approval_policies"), "{message}");
    }
}

#[test]
fn feature_aliases_merge_in_both_priority_orders() {
    for (low_key, high_key) in [
        ("features", "feature_requirements"),
        ("feature_requirements", "features"),
    ] {
        let low = layer(
            "low",
            "Low",
            &format!("[{low_key}]\nlow = true\nshared = false"),
        );
        let high = layer(
            "high",
            "High",
            &format!("[{high_key}]\nhigh = true\nshared = true"),
        );
        let source = RequirementSource::composite([high.source.clone(), low.source.clone()]);
        let output = super::compose_requirements([low, high])
            .expect("aliases share one merge key")
            .expect("requirements present");
        assert_eq!(
            output
                .feature_requirements
                .as_ref()
                .expect("features")
                .source,
            source
        );
        assert_eq!(
            output.into_toml(),
            expected_requirements("[features]\nlow = true\nhigh = true\nshared = true")
        );
    }
}

#[test]
fn duplicate_feature_spellings_in_one_layer_are_rejected() {
    let entry = layer(
        "bad",
        "Bad layer",
        "[features]\na = true\n[feature_requirements]\nb = false",
    );
    let source = entry.source.clone();
    let err = super::compose_requirements([entry]).expect_err("duplicate fields must fail");
    let RequirementsCompositionError::Parse {
        layer_source,
        message,
    } = err
    else {
        panic!("expected layer parse error: {err}");
    };
    assert_eq!(layer_source, source);
    assert!(message.contains("duplicate field `features`"), "{message}");
}

#[test]
fn top_level_values_use_toml_priority() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
allowed_approval_policies = ["on-request"]
allowed_sandbox_modes = ["workspace-write"]
default_permissions = ":workspace"
allow_remote_control = true

[allowed_permission_profiles]
":read-only" = true
":workspace" = true
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
allowed_approval_policies = ["never"]
allowed_sandbox_modes = ["read-only"]
default_permissions = ":read-only"
allow_remote_control = false

[allowed_permission_profiles]
":danger-full-access" = false
":workspace" = false
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
allowed_approval_policies = ["never"]
allowed_sandbox_modes = ["read-only"]
default_permissions = ":read-only"
allow_remote_control = false

[allowed_permission_profiles]
":danger-full-access" = false
":read-only" = true
":workspace" = false
"#
        )
    );
}

#[test]
fn new_thread_model_defaults_use_toml_priority() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[models.new_thread]
model = "low-priority-model"
model_reasoning_effort = "low"
service_tier = "flex"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[models.new_thread]
model = "high-priority-model"
model_reasoning_effort = "high"
service_tier = "priority"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[models.new_thread]
model = "high-priority-model"
model_reasoning_effort = "high"
service_tier = "priority"
"#
        )
    );
}

#[test]
fn composition_strategy_applies_to_non_cloud_layers() {
    let mdm_source = RequirementSource::MdmManagedPreferences {
        domain: "com.openai.codex".to_string(),
        key: "requirements_toml_base64".to_string(),
    };
    let system_file = "C:\\requirements.toml";
    let system_source = RequirementSource::SystemRequirementsToml {
        file: AbsolutePathBuf::from_absolute_path(system_file).expect("absolute path"),
    };
    let high_path = "C:\\secret";
    let low_path = "C:\\other-secret";

    let composed = compose_requirements_for_hostname(
        vec![
            RequirementsLayerEntry::from_toml(
                system_source,
                format!(
                    r#"
allowed_approval_policies = ["on-request"]
allow_remote_control = true

[features]
shared = false
system = true

[[rules.prefix_rules]]
pattern = [{{ token = "npm" }}]
decision = "prompt"

[permissions.filesystem]
deny_read = [{low_path:?}]
"#
                ),
            ),
            RequirementsLayerEntry::from_toml(
                mdm_source.clone(),
                format!(
                    r#"
allowed_approval_policies = ["never"]
allow_remote_control = false

[features]
shared = true

[[rules.prefix_rules]]
pattern = [{{ token = "git" }}]
decision = "forbidden"

[permissions.filesystem]
deny_read = [{high_path:?}]
"#
                ),
            ),
        ],
        /*hostname*/ None,
    )
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed.clone().into_toml(),
        expected_requirements(format!(
            r#"
allowed_approval_policies = ["never"]
allow_remote_control = false

[features]
shared = true
system = true

[[rules.prefix_rules]]
pattern = [{{ token = "git" }}]
decision = "forbidden"

[[rules.prefix_rules]]
pattern = [{{ token = "npm" }}]
decision = "prompt"

[permissions.filesystem]
deny_read = [{high_path:?}, {low_path:?}]
"#
        ))
    );
    assert_eq!(
        composed.allowed_approval_policies,
        Some(Sourced::new(
            vec![AskForApproval::Never],
            mdm_source.clone()
        ))
    );
    assert_eq!(
        composed.allow_remote_control,
        Some(Sourced::new(/*value*/ false, mdm_source))
    );
}

#[test]
fn single_regular_layer_keeps_enterprise_managed_source() {
    let composed = compose_requirements_for_hostname(
        vec![layer(
            "req_1",
            "Security baseline",
            r#"
allow_managed_hooks_only = true
"#,
        )],
        /*hostname*/ None,
    )
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed.allow_managed_hooks_only,
        Some(Sourced::new(
            /*value*/ true,
            RequirementSource::EnterpriseManaged {
                id: "req_1".to_string(),
                name: "Security baseline".to_string(),
            },
        ))
    );
}

#[test]
fn regular_toml_merge_recurses_into_tables() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[features]
beta = false
shared = false

[apps.connector_1]
enabled = false

[apps.connector_1.tools.search]
approval_mode = "prompt"

[apps.connector_1.tools.list]
approval_mode = "prompt"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[features]
alpha = true
shared = true

[apps.connector_1]
enabled = true

[apps.connector_1.tools.search]
approval_mode = "approve"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[features]
alpha = true
beta = false
shared = true

[apps.connector_1]
enabled = true

[apps.connector_1.tools.list]
approval_mode = "prompt"

[apps.connector_1.tools.search]
approval_mode = "approve"
"#
        )
    );
}

#[test]
fn merged_table_source_is_composite_in_priority_order() {
    let high_source = RequirementSource::EnterpriseManaged {
        id: "req_high".to_string(),
        name: "High".to_string(),
    };
    let low_source = RequirementSource::EnterpriseManaged {
        id: "req_low".to_string(),
        name: "Low".to_string(),
    };
    let composed = compose_requirements_for_hostname(
        vec![
            RequirementsLayerEntry::from_toml(
                low_source.clone(),
                r#"
[features]
beta = true
"#,
            ),
            RequirementsLayerEntry::from_toml(
                high_source.clone(),
                r#"
[features]
alpha = true
"#,
            ),
        ],
        /*hostname*/ None,
    )
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed.feature_requirements.expect("features"),
        Sourced::new(
            crate::FeatureRequirementsToml {
                entries: BTreeMap::from([("alpha".to_string(), true), ("beta".to_string(), true),]),
            },
            RequirementSource::composite([high_source, low_source]),
        )
    );
}

#[test]
fn mcp_requirements_use_regular_toml_merge() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[mcp_servers.shared.identity]
command = "low-mcp"

[mcp_servers.low.identity]
url = "https://low.example.com/mcp"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[mcp_servers.shared.identity]
command = "high-mcp"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[mcp_servers.low.identity]
url = "https://low.example.com/mcp"

[mcp_servers.shared.identity]
command = "high-mcp"
"#
        )
    );
}

#[test]
fn network_maps_use_regular_toml_merge() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[experimental_network.domains]
"example.com" = "deny"
"low.example.com" = "deny"
"internal.example.com" = "allow"

[experimental_network.unix_sockets]
"/tmp/shared.sock" = "deny"
"/tmp/low.sock" = "allow"
"/tmp/admin.sock" = "allow"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[experimental_network.domains]
"example.com" = "allow"
"high.example.com" = "allow"
"internal.example.com" = "deny"

[experimental_network.unix_sockets]
"/tmp/shared.sock" = "allow"
"/tmp/high.sock" = "allow"
"/tmp/admin.sock" = "deny"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[experimental_network.domains]
"example.com" = "allow"
"high.example.com" = "allow"
"internal.example.com" = "deny"
"low.example.com" = "deny"

[experimental_network.unix_sockets]
"/tmp/admin.sock" = "deny"
"/tmp/high.sock" = "allow"
"/tmp/low.sock" = "allow"
"/tmp/shared.sock" = "allow"
"#
        )
    );
}

#[test]
fn windows_requirements_use_regular_toml_merge() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[windows]
allowed_sandbox_implementations = ["unelevated"]
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[windows]
allowed_sandbox_implementations = ["elevated"]
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[windows]
allowed_sandbox_implementations = ["elevated"]
"#
        )
    );
}

#[test]
fn remote_sandbox_config_is_applied_per_layer() {
    let composed = compose_requirements_for_hostname(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
allowed_sandbox_modes = ["read-only"]
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[[remote_sandbox_config]]
hostname_patterns = ["build-*.example.com"]
allowed_sandbox_modes = ["workspace-write"]
"#,
            ),
        ],
        Some("BUILD-01.EXAMPLE.COM."),
    )
    .expect("compose requirements")
    .expect("requirements present")
    .into_toml();

    assert_eq!(
        composed,
        expected_requirements(
            r#"
allowed_sandbox_modes = ["workspace-write"]
"#
        )
    );
}

#[test]
fn unmatched_remote_sandbox_config_does_not_shadow_lower_layers() {
    let composed = compose_requirements_for_hostname(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
allowed_sandbox_modes = ["read-only"]
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[[remote_sandbox_config]]
hostname_patterns = ["mac-*.example.com"]
allowed_sandbox_modes = ["workspace-write"]
"#,
            ),
        ],
        Some("linux-01.example.com"),
    )
    .expect("compose requirements")
    .expect("requirements present")
    .into_toml();

    assert_eq!(
        composed,
        expected_requirements(
            r#"
allowed_sandbox_modes = ["read-only"]
"#
        )
    );
}

#[test]
fn hostname_resolver_is_not_called_without_remote_sandbox_config() {
    let calls = Cell::<usize>::default();
    let composed = compose_requirements_with_hostname_resolver(
        vec![layer(
            "req",
            "No remote selector",
            r#"
allowed_sandbox_modes = ["read-only"]
"#,
        )],
        || {
            calls.set(calls.get() + 1);
            Some("build-01.example.com".to_string())
        },
    )
    .expect("compose requirements")
    .expect("requirements present")
    .into_toml();

    assert_eq!(calls.get(), 0);
    assert_eq!(
        composed,
        expected_requirements(
            r#"
allowed_sandbox_modes = ["read-only"]
"#
        )
    );
}

#[test]
fn hostname_resolver_is_not_called_for_empty_remote_sandbox_config() {
    let calls = Cell::<usize>::default();
    let composed = compose_requirements_with_hostname_resolver(
        [
            layer("low", "Low", "allowed_sandbox_modes = ['read-only']"),
            layer("high", "High", "remote_sandbox_config = []"),
        ],
        || {
            calls.set(calls.get() + 1);
            Some("build.example.com".to_string())
        },
    )
    .expect("compose requirements")
    .expect("requirements present");
    assert_eq!(calls.get(), 0);
    assert_eq!(
        composed.into_toml(),
        expected_requirements("allowed_sandbox_modes = ['read-only']")
    );
}

#[test]
fn hostname_resolver_is_called_once_for_multiple_remote_sandbox_layers() {
    let calls = Cell::<usize>::default();
    let composed = compose_requirements_with_hostname_resolver(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[[remote_sandbox_config]]
hostname_patterns = ["build-*.example.com"]
allowed_sandbox_modes = ["read-only"]
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[[remote_sandbox_config]]
hostname_patterns = ["build-*.example.com"]
allowed_sandbox_modes = ["workspace-write"]
"#,
            ),
        ],
        || {
            calls.set(calls.get() + 1);
            Some("build-01.example.com".to_string())
        },
    )
    .expect("compose requirements")
    .expect("requirements present")
    .into_toml();

    assert_eq!(calls.get(), 1);
    assert_eq!(
        composed,
        expected_requirements(
            r#"
allowed_sandbox_modes = ["workspace-write"]
"#
        )
    );
}

#[test]
fn rules_are_appended_in_priority_order() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[[rules.prefix_rules]]
pattern = [{ token = "npm" }]
decision = "prompt"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[[rules.prefix_rules]]
pattern = [{ token = "git" }]
decision = "forbidden"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[[rules.prefix_rules]]
pattern = [{ token = "git" }]
decision = "forbidden"

[[rules.prefix_rules]]
pattern = [{ token = "npm" }]
decision = "prompt"
"#
        )
    );
}

#[test]
fn hooks_append_groups_and_reject_conflicting_managed_dirs() {
    let composed = compose_with_hook_directory_field(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[hooks]
managed_dir = "/managed/hooks"

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[hooks]
managed_dir = "/managed/hooks"

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"
"#,
            ),
        ],
        HookDirectoryField::ManagedDir,
    )
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[hooks]
managed_dir = "/managed/hooks"

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#
        )
    );

    let err = compose_with_hook_directory_field(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[hooks]
managed_dir = "/managed/low"
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[hooks]
managed_dir = "/managed/high"
"#,
            ),
        ],
        HookDirectoryField::ManagedDir,
    )
    .expect_err("conflicting managed dirs should fail closed");
    assert!(err.to_string().contains("hooks.managed_dir"));
    assert_conflict_sources(
        err,
        "hooks.managed_dir",
        "req_high",
        "High",
        "req_low",
        "Low",
    );
}

#[test]
fn active_windows_managed_dir_conflicts_fail_closed() {
    let err = compose_with_hook_directory_field(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[hooks]
windows_managed_dir = 'C:\managed\low'
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[hooks]
windows_managed_dir = 'C:\managed\high'
"#,
            ),
        ],
        HookDirectoryField::WindowsManagedDir,
    )
    .expect_err("conflicting windows managed dirs should fail closed");

    assert!(err.to_string().contains("hooks.windows_managed_dir"));
    assert_conflict_sources(
        err,
        "hooks.windows_managed_dir",
        "req_high",
        "High",
        "req_low",
        "Low",
    );
}

fn assert_conflict_sources(
    error: RequirementsCompositionError,
    expected_field: &str,
    existing_id: &str,
    existing_name: &str,
    incoming_id: &str,
    incoming_name: &str,
) {
    let RequirementsCompositionError::Conflict {
        field,
        existing_source,
        incoming_source,
        ..
    } = error
    else {
        panic!("expected conflict: {error}");
    };
    assert_eq!(field, expected_field);
    assert_eq!(
        existing_source,
        layer(existing_id, existing_name, "").source
    );
    assert_eq!(
        incoming_source,
        layer(incoming_id, incoming_name, "").source
    );
}

#[test]
fn default_hook_conflict_reports_directory_owner_instead_of_event_source() {
    let err = super::compose_requirements([
        layer(
            "low",
            "Conflicting directory",
            "[hooks]\nwindows_managed_dir = 'C:\\low'",
        ),
        layer(
            "middle",
            "Directory owner",
            "[hooks]\nwindows_managed_dir = 'C:\\middle'",
        ),
        layer(
            "high",
            "Events only",
            r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"
"#,
        ),
    ])
    .expect_err("default composition must reject conflicting Windows directories");
    assert_conflict_sources(
        err,
        "hooks.windows_managed_dir",
        "middle",
        "Directory owner",
        "low",
        "Conflicting directory",
    );
}

#[test]
fn inactive_hook_dir_conflicts_do_not_fail_composition() {
    let composed = compose_with_hook_directory_field(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[hooks]
managed_dir = "/managed/hooks"
windows_managed_dir = 'C:\managed\low'

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[hooks]
managed_dir = "/managed/hooks"
windows_managed_dir = 'C:\managed\high'

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"
"#,
            ),
        ],
        HookDirectoryField::ManagedDir,
    )
    .expect("inactive windows managed dir conflict should not fail")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[hooks]
managed_dir = "/managed/hooks"
windows_managed_dir = 'C:\managed\high'

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#
        )
    );

    let composed = compose_with_hook_directory_field(
        vec![
            layer(
                "req_low",
                "Low",
                r#"
[hooks]
managed_dir = "/managed/low"
windows_managed_dir = 'C:\managed\hooks'

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#,
            ),
            layer(
                "req_high",
                "High",
                r#"
[hooks]
managed_dir = "/managed/high"
windows_managed_dir = 'C:\managed\hooks'

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"
"#,
            ),
        ],
        HookDirectoryField::WindowsManagedDir,
    )
    .expect("inactive managed dir conflict should not fail")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[hooks]
managed_dir = "/managed/high"
windows_managed_dir = 'C:\managed\hooks'

[[hooks.PreToolUse]]
matcher = "Edit"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "high"

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "low"
"#
        )
    );
}

#[test]
fn permissions_deny_read_unions_while_profiles_use_regular_toml_merge() {
    let high_path = "C:\\secret";
    let low_path = "C:\\other-secret";
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            &format!(
                r#"
[permissions.filesystem]
deny_read = [{high_path:?}, {low_path:?}]

[permissions.managed-standard]
description = "Low profile"
extends = ":workspace"
"#
            ),
        ),
        layer(
            "req_high",
            "High",
            &format!(
                r#"
[permissions.filesystem]
deny_read = [{high_path:?}]

[permissions.managed-standard]
description = "High profile"
"#
            ),
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(format!(
            r#"
[permissions.filesystem]
deny_read = [{high_path:?}, {low_path:?}]

[permissions.managed-standard]
description = "High profile"
extends = ":workspace"
"#
        ))
    );
}

#[test]
fn deny_read_only_layers_do_not_leave_empty_permissions_tables() {
    let path = "C:\\secret";
    let composed = compose(vec![layer(
        "req_high",
        "High",
        &format!(
            r#"
[permissions.filesystem]
deny_read = [{path:?}]
"#
        ),
    )])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(format!(
            r#"
[permissions.filesystem]
deny_read = [{path:?}]
"#
        ))
    );
}

#[test]
fn deny_read_union_tracks_only_contributing_layers_and_preserves_order() {
    let high = layer(
        "high",
        "High",
        r#"
[permissions.filesystem]
deny_read = ['C:\high', 'C:\shared', 'C:\high']
"#,
    );
    let duplicate = layer(
        "duplicate",
        "Duplicate only",
        r#"
[permissions.filesystem]
deny_read = ['C:\shared', 'C:\high']
"#,
    );
    let low = layer(
        "low",
        "Low",
        r#"
[permissions.filesystem]
deny_read = ['C:\shared', 'C:\low', 'C:\other', 'C:\low']
"#,
    );
    let source = RequirementSource::composite([high.source.clone(), low.source.clone()]);
    let output = super::compose_requirements([
        layer("empty", "Empty", "[permissions.filesystem]\ndeny_read = []"),
        low,
        duplicate,
        high,
    ])
    .expect("compose requirements")
    .expect("requirements present");
    assert_eq!(
        output.permissions.as_ref().expect("permissions").source,
        source
    );
    assert_eq!(
        output.into_toml(),
        expected_requirements(
            r#"
[permissions.filesystem]
deny_read = ['C:\high', 'C:\shared', 'C:\low', 'C:\other']
"#
        )
    );
}

#[test]
fn parse_error_names_layer() {
    let err = compose(vec![layer(
        "req_bad",
        "Bad layer",
        "allowed_approval_policies = [1]",
    )])
    .expect_err("invalid layer should fail");

    assert!(err.to_string().contains("Bad layer (req_bad)"));
    assert!(err.to_string().contains("allowed_approval_policies"));
}

#[test]
fn marketplace_allowed_sources_use_default_toml_merge() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.shared]
source = "git"
url = "https://github.com/example/old.git"
ref = "main"

[marketplaces.allowed_sources.other]
source = "git"
url = "https://github.com/example/other.git"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[marketplaces.allowed_sources.shared]
ref = "release"
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.shared]
source = "git"
url = "https://github.com/example/old.git"
ref = "release"

[marketplaces.allowed_sources.other]
source = "git"
url = "https://github.com/example/other.git"
"#,
        )
    );
}

#[test]
fn marketplace_source_switch_uses_default_toml_merge() {
    let composed = compose(vec![
        layer(
            "req_low",
            "Low",
            r#"
[marketplaces.allowed_sources.company]
source = "git"
url = "https://github.com/example/plugins.git"
ref = "main"
"#,
        ),
        layer(
            "req_high",
            "High",
            r#"
[marketplaces.allowed_sources.company]
source = "host_pattern"
host_pattern = '^github\.example\.com$'
"#,
        ),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[marketplaces.allowed_sources.company]
source = "host_pattern"
url = "https://github.com/example/plugins.git"
ref = "main"
host_pattern = '^github\.example\.com$'
"#,
        )
    );
}

#[test]
fn marketplace_allowed_source_rejects_unknown_fields() {
    let err = compose(vec![layer(
        "req_bad",
        "Bad marketplace layer",
        r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.invalid]
source = "git"
url = "https://github.com/example/plugins.git"
reff = "main"
"#,
    )])
    .expect_err("invalid marketplace rule should fail");

    assert!(err.to_string().contains("Bad marketplace layer (req_bad)"));
    assert!(err.to_string().contains("unknown field `reff`"));
}

#[test]
fn local_marketplace_path_is_not_resolved_during_requirements_merge() {
    let base_dir = TempDir::new().expect("create requirements base directory");
    let base_dir = AbsolutePathBuf::try_from(base_dir.path().to_path_buf())
        .expect("absolute requirements base directory");
    let composed = compose(vec![
        layer(
            "req_local",
            "Local marketplace path",
            r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.local]
source = "local"
path = "../plugins"
"#,
        )
        .with_base_dir(base_dir),
    ])
    .expect("compose requirements")
    .expect("requirements present");

    assert_eq!(
        composed,
        expected_requirements(
            r#"
[marketplaces]
restrict_to_allowed_sources = true

[marketplaces.allowed_sources.local]
source = "local"
path = "../plugins"
"#,
        )
    );
}
