use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::environment_context::FileSystemContext;
use crate::context::environment_context::NetworkContext;
use crate::context::environment_context::push_xml_escaped_attribute;
use crate::context::environment_context::push_xml_escaped_text;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnContext;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

/// Environment values visible to the model.
#[derive(Clone, Debug, Default)]
pub(crate) struct EnvironmentsState {
    environments: BTreeMap<String, EnvironmentState>,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<NetworkContext>,
    filesystem: Option<FileSystemContext>,
    subagents: Option<String>,
}

impl EnvironmentsState {
    pub(crate) fn from_turn_context_with_environments(
        turn_context: &TurnContext,
        environments: &TurnEnvironmentSnapshot,
    ) -> Self {
        Self {
            environments: environment_states(environments),
            current_date: turn_context.current_date.clone(),
            timezone: turn_context.timezone.clone(),
            network: network_from_turn_context(turn_context),
            filesystem: Some(FileSystemContext::from_permission_profile(
                &turn_context.permission_profile,
                turn_context.effective_workspace_roots(),
            )),
            subagents: None,
        }
    }

    pub(crate) fn with_subagents(mut self, subagents: String) -> Self {
        if !subagents.is_empty() {
            let mut budget = codex_context_fragments::ModelContextBudget::new(1024);
            let mut lines = Vec::new();
            for line in subagents.lines() {
                if !budget.try_take(line) {
                    lines
                        .push("Additional subagents omitted; use list_agents for current details.");
                    break;
                }
                lines.push(line);
            }
            self.subagents = Some(lines.join("\n"));
        }
        self
    }

    fn rendered_full(&self) -> RenderedEnvironments {
        RenderedEnvironments {
            updates: self
                .environments
                .iter()
                .map(|(id, environment)| {
                    (
                        id.clone(),
                        EnvironmentUpdate::Current(environment.clone(), false),
                    )
                })
                .collect(),
            legacy_single: is_legacy_single(&self.environments),
            replace_all: false,
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            network: self.network.as_ref().map(NetworkContext::render),
            filesystem: self.filesystem.as_ref().map(FileSystemContext::render),
            subagents: self.subagents.clone(),
        }
    }
}

impl WorldStateSection for EnvironmentsState {
    const ID: &'static str = "environments";
    type Snapshot = EnvironmentsSnapshot;

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && Self::matches_text(text)
    }

    fn required() -> bool {
        true
    }

    fn records_delivery() -> bool {
        true
    }

    fn snapshot(&self) -> Self::Snapshot {
        EnvironmentsSnapshot {
            environments: self
                .environments
                .iter()
                .map(|(id, environment)| {
                    (
                        id.clone(),
                        EnvironmentSnapshot {
                            cwd: environment.cwd.inferred_native_path_string(),
                            status: environment.status,
                            shell: environment.shell.clone(),
                            os: environment.os.clone(),
                        },
                    )
                })
                .collect(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            network: self.network.as_ref().map(NetworkContext::render),
            filesystem: self.filesystem.as_ref().map(FileSystemContext::render),
            subagents: self.subagents.clone(),
        }
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        let empty = EnvironmentsSnapshot::default();
        let replace_all = matches!(previous, PreviousSectionState::Unknown);
        let replace_visible_context = !matches!(previous, PreviousSectionState::Absent);
        let previous = match previous {
            PreviousSectionState::Known(previous) => previous,
            PreviousSectionState::Absent | PreviousSectionState::Unknown => &empty,
        };
        let turn_context_values_changed = current.current_date != previous.current_date
            || current.timezone != previous.timezone
            || current.network != previous.network
            || current.filesystem != previous.filesystem;
        let subagents_changed = current.subagents != previous.subagents;
        let mut updates = self
            .environments
            .iter()
            .filter(|(id, _)| {
                let environment = &current.environments[*id];
                previous
                    .environments
                    .get(*id)
                    .is_none_or(|previous| !environment.has_same_diff_value(previous))
            })
            .map(|(id, environment)| {
                let mut environment = environment.clone();
                if environment.shell.is_none()
                    && previous
                        .environments
                        .get(id)
                        .is_some_and(|previous| previous.shell.is_some())
                {
                    environment.shell = Some("unknown".to_string());
                }
                if environment.os.is_none()
                    && previous
                        .environments
                        .get(id)
                        .is_some_and(|previous| previous.os.is_some())
                {
                    environment.os = Some("unknown".to_string());
                }
                let became_available = environment.status == EnvironmentStatus::Available
                    && previous
                        .environments
                        .get(id)
                        .is_some_and(|previous| previous.status == EnvironmentStatus::Starting);
                (
                    id.clone(),
                    EnvironmentUpdate::Current(environment, became_available),
                )
            })
            .collect::<BTreeMap<_, _>>();
        updates.extend(
            previous
                .environments
                .keys()
                .filter(|id| !self.environments.contains_key(*id))
                .map(|id| (id.clone(), EnvironmentUpdate::Unavailable)),
        );
        let legacy_single = is_legacy_single(&self.environments)
            && updates
                .values()
                .all(|update| matches!(update, EnvironmentUpdate::Current(..)));
        (replace_all || !updates.is_empty() || turn_context_values_changed || subagents_changed)
            .then(|| {
                // Stable-context projection replaces the previous environment
                // block. Include unchanged current fields as well as removals.
                for (id, environment) in &self.environments {
                    updates
                        .entry(id.clone())
                        .or_insert_with(|| EnvironmentUpdate::Current(environment.clone(), false));
                }
                Box::new(RenderedEnvironments {
                    updates,
                    legacy_single,
                    // This block contains all current facts. Make omission semantics explicit
                    // so its delivery remains sufficient if an earlier clearing delta is lost.
                    replace_all: replace_visible_context,
                    current_date: changed_value(
                        &current.current_date,
                        &previous.current_date,
                        "unknown",
                    ),
                    timezone: changed_value(&current.timezone, &previous.timezone, "unknown"),
                    network: changed_value(
                        &current.network,
                        &previous.network,
                        "<network status=\"unspecified\" />",
                    ),
                    filesystem: changed_value(
                        &current.filesystem,
                        &previous.filesystem,
                        "<filesystem status=\"unspecified\" />",
                    ),
                    subagents: changed_value(&current.subagents, &previous.subagents, "none"),
                }) as Box<dyn ContextualUserFragment>
            })
    }
}

impl ContextualUserFragment for EnvironmentsState {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        environment_context_markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(self.rendered_full().body().into_owned())
    }
}

struct RenderedEnvironments {
    updates: BTreeMap<String, EnvironmentUpdate>,
    legacy_single: bool,
    replace_all: bool,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<String>,
    filesystem: Option<String>,
    subagents: Option<String>,
}

enum EnvironmentUpdate {
    Current(EnvironmentState, bool),
    Unavailable,
}

impl ContextualUserFragment for RenderedEnvironments {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        environment_context_markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned({
            let mut rendered = "\n".to_string();
            if self.replace_all {
                rendered.push_str("  This environment context replaces all previously provided environment context. Unlisted environments are unavailable; omitted fields are unspecified; omitted subagents means none.\n");
            }
            if self.legacy_single {
                if let Some(EnvironmentUpdate::Current(environment, report_available)) =
                    self.updates.values().next()
                {
                    push_environment_values(&mut rendered, environment, "  ", *report_available);
                }
            } else if !self.updates.is_empty() {
                rendered.push_str("  <environments>\n");
                for (id, update) in &self.updates {
                    match update {
                        EnvironmentUpdate::Current(environment, report_available) => {
                            rendered.push_str("    <environment id=\"");
                            push_xml_escaped_attribute(&mut rendered, id);
                            rendered.push('"');
                            rendered.push_str(">\n");
                            push_environment_values(
                                &mut rendered,
                                environment,
                                "      ",
                                *report_available,
                            );
                            rendered.push_str("    </environment>\n");
                        }
                        EnvironmentUpdate::Unavailable => {
                            rendered.push_str("    <environment id=\"");
                            push_xml_escaped_attribute(&mut rendered, id);
                            rendered.push_str("\" status=\"unavailable\" />\n");
                        }
                    }
                }
                rendered.push_str("  </environments>\n");
            }
            push_optional_element(&mut rendered, "current_date", self.current_date.as_deref());
            push_optional_element(&mut rendered, "timezone", self.timezone.as_deref());
            if let Some(network) = &self.network {
                rendered.push_str("  ");
                rendered.push_str(network);
                rendered.push('\n');
            }
            if let Some(filesystem) = &self.filesystem {
                rendered.push_str("  ");
                rendered.push_str(filesystem);
                rendered.push('\n');
            }
            if let Some(subagents) = &self.subagents {
                rendered.push_str("  <subagents>\n");
                for line in subagents.lines() {
                    rendered.push_str("    ");
                    push_xml_escaped_text(&mut rendered, line);
                    rendered.push('\n');
                }
                rendered.push_str("  </subagents>\n");
            }
            rendered
        })
    }
}

fn changed_value(
    current: &Option<String>,
    previous: &Option<String>,
    cleared: &str,
) -> Option<String> {
    current
        .clone()
        .or_else(|| previous.as_ref().map(|_| cleared.to_string()))
}

fn push_environment_values(
    rendered: &mut String,
    environment: &EnvironmentState,
    indent: &str,
    report_available: bool,
) {
    rendered.push_str(indent);
    rendered.push_str("<cwd>");
    push_xml_escaped_text(rendered, &environment.cwd.inferred_native_path_string());
    rendered.push_str("</cwd>\n");
    if let Some(os) = &environment.os {
        rendered.push_str(indent);
        rendered.push_str("<os>");
        push_xml_escaped_text(rendered, os);
        rendered.push_str("</os>\n");
    }
    if environment.status == EnvironmentStatus::Starting {
        rendered.push_str(indent);
        rendered.push_str("<status>starting</status>\n");
    } else if report_available {
        rendered.push_str(indent);
        rendered.push_str("<status>available</status>\n");
    }
    if let Some(shell) = &environment.shell {
        rendered.push_str(indent);
        rendered.push_str("<shell>");
        push_xml_escaped_text(rendered, shell);
        rendered.push_str("</shell>\n");
    }
}

fn push_optional_element(rendered: &mut String, name: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    rendered.push_str("  <");
    rendered.push_str(name);
    rendered.push('>');
    push_xml_escaped_text(rendered, value);
    rendered.push_str("</");
    rendered.push_str(name);
    rendered.push_str(">\n");
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentState {
    cwd: PathUri,
    status: EnvironmentStatus,
    shell: Option<String>,
    os: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct EnvironmentsSnapshot {
    environments: BTreeMap<String, EnvironmentSnapshot>,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<String>,
    filesystem: Option<String>,
    subagents: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct EnvironmentSnapshot {
    cwd: String,
    status: EnvironmentStatus,
    shell: Option<String>,
    #[serde(default)]
    os: Option<String>,
}

impl EnvironmentSnapshot {
    fn has_same_diff_value(&self, other: &Self) -> bool {
        self.cwd == other.cwd
            && self.status == other.status
            && self.shell == other.shell
            && self.os == other.os
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvironmentStatus {
    Starting,
    Available,
}

fn environment_states(snapshot: &TurnEnvironmentSnapshot) -> BTreeMap<String, EnvironmentState> {
    let mut environments = snapshot
        .turn_environments
        .iter()
        .map(|environment| {
            (
                environment.environment_id.clone(),
                EnvironmentState {
                    cwd: environment.cwd().clone(),
                    os: environment.operating_system.clone(),
                    status: EnvironmentStatus::Available,
                    shell: environment
                        .shell
                        .as_ref()
                        .map(|shell| shell.name().to_string()),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for environment in &snapshot.starting {
        environments
            .entry(environment.selection.environment_id.clone())
            .or_insert_with(|| EnvironmentState {
                cwd: environment.selection.cwd.clone(),
                os: None,
                status: EnvironmentStatus::Starting,
                shell: None,
            });
    }
    environments
}

fn is_legacy_single(environments: &BTreeMap<String, EnvironmentState>) -> bool {
    environments.len() == 1
        && environments
            .values()
            .all(|environment| environment.status == EnvironmentStatus::Available)
}

fn environment_context_markers() -> (&'static str, &'static str) {
    (
        codex_protocol::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG,
        codex_protocol::protocol::ENVIRONMENT_CONTEXT_CLOSE_TAG,
    )
}

fn network_from_turn_context(turn_context: &TurnContext) -> Option<NetworkContext> {
    let network_requirements = turn_context
        .config
        .config_layer_stack
        .requirements()
        .network
        .as_ref()?;
    let enabled = turn_context
        .config
        .permissions
        .network
        .as_ref()
        .is_some_and(crate::config::NetworkProxySpec::enabled);

    Some(NetworkContext::new(
        enabled,
        network_requirements
            .domains
            .as_ref()
            .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains)
            .unwrap_or_default(),
        network_requirements
            .domains
            .as_ref()
            .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains)
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "environment_render_tests.rs"]
mod render_tests;

#[cfg(test)]
#[path = "environment_retention_tests.rs"]
mod retention_tests;
