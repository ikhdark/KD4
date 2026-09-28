use std::io;

use codex_exec_server::ExecutorFileSystem;
use codex_protocol::protocol::Product;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use noyalib::compat::serde_yaml;

use crate::model::SkillDependencies;
use crate::model::SkillPolicy;

use super::MAX_QUALIFIED_NAME_LEN;
use super::ParsedSkillFrontmatter;
use super::SkillMetadataFile;
use super::discovery::DirectorySymlinkPolicy;
use super::discovery::DiscoveredSkill;
use super::discovery::HiddenDirectoryPolicy;
use super::discovery::MAX_CONCURRENT_SKILL_LOADS;
use super::discovery::SkillDiscoveryOptions;
use super::discovery::SkillMetadataDiscovery;
use super::discovery::discover_skills;
use super::namespace::SkillNamespaceResolver;
use super::parse_skill_frontmatter_metadata_inner;
use super::resolve_dependencies;
use super::resolve_policy;
use super::sanitize_single_line;
use super::validate_len;

struct ParsedEnvironmentSkill {
    path_to_skills_md: PathUri,
    base_name: String,
    description: String,
    short_description: Option<String>,
    dependencies: Option<SkillDependencies>,
    policy: Option<SkillPolicy>,
    warnings: Vec<String>,
    retryable_errors: bool,
}

struct EnvironmentSkillLoadError {
    message: String,
    retryable: bool,
}

impl EnvironmentSkillLoadError {
    fn invalid(error: impl std::fmt::Display) -> Self {
        Self {
            message: error.to_string(),
            retryable: false,
        }
    }
}

#[derive(Default)]
struct LoadedSkillMetadata {
    dependencies: Option<SkillDependencies>,
    policy: Option<SkillPolicy>,
    warnings: Vec<String>,
    retryable_errors: bool,
}

/// URI-native metadata for one skill owned by an execution environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentSkillMetadata {
    pub path_to_skills_md: PathUri,
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub dependencies: Option<SkillDependencies>,
    pub policy: Option<SkillPolicy>,
}

impl EnvironmentSkillMetadata {
    pub fn allows_implicit_invocation(&self) -> bool {
        self.policy
            .as_ref()
            .is_none_or(SkillPolicy::allows_implicit_invocation)
    }

    fn matches_product_restriction(&self, restriction_product: Option<Product>) -> bool {
        self.policy
            .as_ref()
            .is_none_or(|policy| policy.matches_product_restriction(restriction_product))
    }
}

impl ParsedEnvironmentSkill {
    async fn load(
        file_system: &dyn ExecutorFileSystem,
        skill: &DiscoveredSkill,
    ) -> Result<Self, EnvironmentSkillLoadError> {
        let (contents, discovered_metadata) = match &skill.metadata {
            SkillMetadataDiscovery::Present(metadata_path) => {
                let (contents, metadata) = tokio::join!(
                    read_skill_contents(file_system, &skill.path),
                    read_skill_metadata(file_system, metadata_path),
                );
                (contents?, metadata)
            }
            SkillMetadataDiscovery::Absent | SkillMetadataDiscovery::Probe(_) => (
                read_skill_contents(file_system, &skill.path).await?,
                LoadedSkillMetadata::default(),
            ),
        };
        let ParsedSkillFrontmatter {
            name: base_name,
            description,
            short_description,
        } = parse_skill_frontmatter_metadata_inner(&contents, || default_skill_name(&skill.path))
            .map_err(EnvironmentSkillLoadError::invalid)?;
        let LoadedSkillMetadata {
            dependencies,
            policy,
            warnings,
            retryable_errors,
        } = match &skill.metadata {
            SkillMetadataDiscovery::Present(_) | SkillMetadataDiscovery::Absent => {
                discovered_metadata
            }
            SkillMetadataDiscovery::Probe(metadata_path) => {
                probe_skill_metadata(file_system, metadata_path).await
            }
        };

        Ok(Self {
            path_to_skills_md: skill.path.clone(),
            base_name,
            description,
            short_description,
            dependencies,
            policy,
            warnings,
            retryable_errors,
        })
    }
}

#[derive(Debug, Default)]
pub struct EnvironmentSkillLoadOutcome {
    pub skills: Vec<EnvironmentSkillMetadata>,
    pub warnings: Vec<String>,
    /// I/O prevented a complete observation; a later turn may retry without discarding siblings.
    pub retryable_errors: bool,
}

/// Discovers skills without converting environment-owned paths to host paths.
#[tracing::instrument(
    name = "skills.environment.load",
    level = "info",
    skip_all,
    fields(skill_count = tracing::field::Empty)
)]
pub async fn load_environment_skills_from_root(
    file_system: &dyn ExecutorFileSystem,
    root: &PathUri,
    restriction_product: Option<Product>,
) -> EnvironmentSkillLoadOutcome {
    let mut outcome = EnvironmentSkillLoadOutcome::default();
    let discovery = discover_skills(
        file_system,
        root,
        // Preserve environment discovery behavior by following directory aliases and including
        // hidden directories exposed by the executor.
        SkillDiscoveryOptions {
            directory_symlinks: DirectorySymlinkPolicy::Follow,
            hidden_directories: HiddenDirectoryPolicy::Include,
        },
    )
    .await;
    tracing::Span::current().record("skill_count", discovery.skills.len());
    outcome.retryable_errors = discovery.retryable_errors;
    outcome.warnings.extend(discovery.warnings);
    if discovery.skills.is_empty() {
        return outcome;
    }

    let skill_paths = discovery
        .skills
        .iter()
        .map(|skill| skill.path.clone())
        .collect::<Vec<_>>();
    let namespace_resolver = SkillNamespaceResolver::discover(
        file_system,
        root,
        &skill_paths,
        discovery.plugin_roots,
        discovery.namespace_roots,
    );

    // Remote executors can multiplex these independent per-skill reads, so polling a bounded
    // number together allows the I/O for each skill and its metadata to happen concurrently.
    let skill_results = futures::stream::iter(discovery.skills)
        .map(|skill| {
            let path = skill.path.clone();
            async move {
                (
                    path,
                    ParsedEnvironmentSkill::load(file_system, &skill).await,
                )
            }
        })
        .buffered(MAX_CONCURRENT_SKILL_LOADS)
        .collect::<Vec<_>>();
    let (namespace_resolver, skill_results) = tokio::join!(namespace_resolver, skill_results);

    for (path, result) in skill_results {
        let result = result.and_then(|skill| {
            outcome.warnings.extend(skill.warnings);
            outcome.retryable_errors |= skill.retryable_errors;
            let name = namespace_resolver
                .for_skill(root, &skill.path_to_skills_md)
                .qualify(&skill.base_name);
            validate_len(&name, MAX_QUALIFIED_NAME_LEN, "qualified name")
                .map_err(EnvironmentSkillLoadError::invalid)?;

            Ok(EnvironmentSkillMetadata {
                path_to_skills_md: skill.path_to_skills_md,
                name,
                description: skill.description,
                short_description: skill.short_description,
                dependencies: skill.dependencies,
                policy: skill.policy,
            })
        });
        match result {
            Ok(skill) if skill.matches_product_restriction(restriction_product) => {
                outcome.skills.push(skill);
            }
            Ok(_) => {}
            Err(error) => {
                outcome.retryable_errors |= error.retryable;
                outcome.warnings.push(format!(
                    "Failed to load environment skill at {path}: {}",
                    error.message
                ));
            }
        }
    }
    outcome.skills.sort_by(|left, right| {
        left.name.cmp(&right.name).then_with(|| {
            left.path_to_skills_md
                .to_string()
                .cmp(&right.path_to_skills_md.to_string())
        })
    });
    outcome
}

async fn read_skill_contents(
    file_system: &dyn ExecutorFileSystem,
    skill_path: &PathUri,
) -> Result<String, EnvironmentSkillLoadError> {
    file_system
        .read_file_text(skill_path, /*sandbox*/ None)
        .await
        .map_err(|err| EnvironmentSkillLoadError {
            message: format!("failed to read file: {err}"),
            retryable: true,
        })
}

async fn probe_skill_metadata(
    file_system: &dyn ExecutorFileSystem,
    metadata_path: &PathUri,
) -> LoadedSkillMetadata {
    match file_system
        .get_metadata(metadata_path, /*sandbox*/ None)
        .await
    {
        Ok(metadata) if metadata.is_file => {}
        Ok(_) => return LoadedSkillMetadata::default(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return LoadedSkillMetadata::default();
        }
        Err(error) => {
            return LoadedSkillMetadata {
                warnings: vec![format!(
                    "ignoring {metadata_path}: failed to stat metadata: {error}"
                )],
                retryable_errors: true,
                ..Default::default()
            };
        }
    }
    read_skill_metadata(file_system, metadata_path).await
}

async fn read_skill_metadata(
    file_system: &dyn ExecutorFileSystem,
    metadata_path: &PathUri,
) -> LoadedSkillMetadata {
    let contents = match file_system
        .read_file_text(metadata_path, /*sandbox*/ None)
        .await
    {
        Ok(contents) => contents,
        Err(error) => {
            return LoadedSkillMetadata {
                warnings: vec![format!(
                    "ignoring {metadata_path}: failed to read metadata: {error}"
                )],
                retryable_errors: true,
                ..Default::default()
            };
        }
    };
    let parsed: SkillMetadataFile = match serde_yaml::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return LoadedSkillMetadata {
                warnings: vec![format!(
                    "ignoring {metadata_path}: invalid metadata: {error}"
                )],
                ..Default::default()
            };
        }
    };

    let mut diagnostics = Vec::new();
    let dependencies = resolve_dependencies(&mut diagnostics, parsed.dependencies);
    LoadedSkillMetadata {
        dependencies,
        policy: resolve_policy(parsed.policy),
        warnings: diagnostics
            .into_iter()
            .map(|message| format!("{metadata_path}: {message}"))
            .collect(),
        retryable_errors: false,
    }
}

fn default_skill_name(path: &PathUri) -> String {
    path.parent()
        .and_then(|parent| parent.basename())
        .map(|name| sanitize_single_line(&name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "skill".to_string())
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
