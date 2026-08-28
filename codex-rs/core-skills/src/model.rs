use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::sync::Arc;

use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::Product;
pub use codex_protocol::protocol::SkillDependencies;
pub use codex_protocol::protocol::SkillInterface;
use codex_protocol::protocol::SkillScope;
pub use codex_protocol::protocol::SkillToolDependency;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;

const SKILL_CATALOG_ID_DOMAIN: &[u8] = b"codex.skill-catalog-id.v1";
pub const SKILL_CATALOG_LOCATOR_PREFIX: &str = "skill:";

pub fn skill_instruction_role(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::System => "system",
        SkillScope::Admin => "developer",
        SkillScope::Repo | SkillScope::User => "user",
    }
}

pub fn skill_scope_label(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::Repo => "repo",
        SkillScope::User => "user",
        SkillScope::System => "system",
        SkillScope::Admin => "admin",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub interface: Option<SkillInterface>,
    pub dependencies: Option<SkillDependencies>,
    pub policy: Option<SkillPolicy>,
    /// Path to the SKILLS.md file that declares this skill.
    pub path_to_skills_md: AbsolutePathBuf,
    pub scope: SkillScope,
    pub plugin_id: Option<String>,
}

impl SkillMetadata {
    pub fn allows_implicit_invocation(&self) -> bool {
        self.policy
            .as_ref()
            .is_none_or(SkillPolicy::allows_implicit_invocation)
    }

    pub fn matches_product_restriction_for_product(
        &self,
        restriction_product: Option<Product>,
    ) -> bool {
        self.policy
            .as_ref()
            .is_none_or(|policy| policy.matches_product_restriction(restriction_product))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillPolicy {
    pub allow_implicit_invocation: Option<bool>,
    // TODO: Enforce product gating in Codex skill selection/injection instead of only parsing and
    // storing this metadata.
    pub products: Vec<Product>,
}

impl SkillPolicy {
    pub fn allows_implicit_invocation(&self) -> bool {
        self.allow_implicit_invocation.unwrap_or(true)
    }

    pub fn matches_product_restriction(&self, restriction_product: Option<Product>) -> bool {
        self.products.is_empty()
            || restriction_product
                .is_some_and(|product| product.matches_product_restriction(&self.products))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError {
    pub path: AbsolutePathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillLoadOutcome {
    pub skills: Vec<SkillMetadata>,
    pub errors: Vec<SkillError>,
    pub disabled_paths: HashSet<AbsolutePathBuf>,
    pub(crate) skill_roots: Vec<AbsolutePathBuf>,
    pub(crate) skill_root_by_path: Arc<HashMap<AbsolutePathBuf, AbsolutePathBuf>>,
    pub(crate) file_systems_by_skill_path: SkillFileSystemsByPath,
    pub(crate) implicit_skills_by_scripts_dir: Arc<HashMap<AbsolutePathBuf, SkillMetadata>>,
    pub(crate) implicit_skills_by_doc_path: Arc<HashMap<AbsolutePathBuf, SkillMetadata>>,
}

impl SkillLoadOutcome {
    pub fn is_skill_enabled(&self, skill: &SkillMetadata) -> bool {
        !self.disabled_paths.contains(&skill.path_to_skills_md)
    }

    pub fn is_skill_allowed_for_implicit_invocation(&self, skill: &SkillMetadata) -> bool {
        self.is_skill_enabled(skill) && skill.allows_implicit_invocation()
    }

    pub fn allowed_skills_for_implicit_invocation(&self) -> Vec<SkillMetadata> {
        self.skills
            .iter()
            .filter(|skill| self.is_skill_allowed_for_implicit_invocation(skill))
            .cloned()
            .collect()
    }

    pub fn skills_with_enabled(&self) -> impl Iterator<Item = (&SkillMetadata, bool)> {
        self.skills
            .iter()
            .map(|skill| (skill, self.is_skill_enabled(skill)))
    }

    pub(crate) fn file_system_for_skill(
        &self,
        skill: &SkillMetadata,
    ) -> Option<Arc<dyn ExecutorFileSystem>> {
        self.file_systems_by_skill_path
            .get(&skill.path_to_skills_md)
    }
}

/// Immutable snapshot of host-owned skills and the filesystem mapping needed
/// to read each skill through the environment that discovered it.
#[derive(Debug, Clone)]
pub struct HostSkillsSnapshot {
    outcome: Arc<SkillLoadOutcome>,
    skills_by_catalog_id: Arc<HashMap<String, SkillMetadata>>,
}

impl HostSkillsSnapshot {
    pub fn new(outcome: Arc<SkillLoadOutcome>) -> Self {
        let mut skills_by_catalog_id = HashMap::with_capacity(outcome.skills.len());
        for skill in &outcome.skills {
            let catalog_id = skill_catalog_id(skill);
            assert!(
                skills_by_catalog_id
                    .insert(catalog_id.clone(), skill.clone())
                    .is_none(),
                "duplicate deterministic skill catalog ID: {catalog_id}"
            );
        }
        Self {
            outcome,
            skills_by_catalog_id: Arc::new(skills_by_catalog_id),
        }
    }

    pub fn outcome(&self) -> &SkillLoadOutcome {
        self.outcome.as_ref()
    }

    pub async fn read_skill_text(&self, skill: &SkillMetadata) -> io::Result<String> {
        let fs = self
            .outcome
            .file_system_for_skill(skill)
            .unwrap_or_else(|| Arc::clone(&LOCAL_FS));
        let path = PathUri::from_abs_path(&skill.path_to_skills_md);
        fs.read_file_text(&path, /*sandbox*/ None).await
    }

    pub fn resolve_catalog_locator(&self, locator: &str) -> Option<&SkillMetadata> {
        let catalog_id = locator.strip_prefix(SKILL_CATALOG_LOCATOR_PREFIX)?;
        self.skills_by_catalog_id.get(catalog_id)
    }

    pub async fn read_skill_text_by_catalog_locator(&self, locator: &str) -> io::Result<String> {
        let skill = self.resolve_catalog_locator(locator).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown skill catalog locator `{locator}`"),
            )
        })?;
        self.read_skill_text(skill).await
    }
}

pub fn skill_catalog_id(skill: &SkillMetadata) -> String {
    let source_kind = "host";
    let scope = skill_scope_label(skill.scope);
    let plugin_id = skill.plugin_id.as_deref().unwrap_or_default();
    let canonical_host_locator = PathUri::from_abs_path(&skill.path_to_skills_md).to_string();
    let mut hasher = Sha256::new();
    hasher.update(SKILL_CATALOG_ID_DOMAIN);
    for part in [
        source_kind,
        scope,
        plugin_id,
        canonical_host_locator.as_str(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod catalog_id_tests {
    use super::*;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;

    #[test]
    fn skill_policy_owns_invocation_and_product_defaults() {
        let unrestricted = SkillPolicy::default();
        assert!(unrestricted.allows_implicit_invocation());
        assert!(unrestricted.matches_product_restriction(None));

        let restricted = SkillPolicy {
            allow_implicit_invocation: Some(false),
            products: vec![Product::Codex],
        };
        assert!(!restricted.allows_implicit_invocation());
        assert!(restricted.matches_product_restriction(Some(Product::Codex)));
        assert!(!restricted.matches_product_restriction(Some(Product::Chatgpt)));
        assert!(!restricted.matches_product_restriction(None));
    }

    #[test]
    fn skill_metadata_uses_shared_protocol_types() {
        fn accept_shared(
            interface: codex_protocol::protocol::SkillInterface,
            dependencies: codex_protocol::protocol::SkillDependencies,
        ) -> (
            codex_protocol::protocol::SkillInterface,
            codex_protocol::protocol::SkillDependencies,
        ) {
            (interface, dependencies)
        }

        let interface = SkillInterface {
            display_name: Some("Example".to_string()),
            short_description: None,
            icon_small: None,
            icon_large: None,
            brand_color: None,
            default_prompt: None,
        };
        let dependencies = SkillDependencies {
            tools: vec![SkillToolDependency {
                r#type: "mcp".to_string(),
                value: "example".to_string(),
                description: None,
                transport: None,
                command: None,
                url: None,
            }],
        };

        let (shared_interface, shared_dependencies) =
            accept_shared(interface.clone(), dependencies.clone());
        assert_eq!(shared_interface, interface);
        assert_eq!(shared_dependencies, dependencies);
    }

    fn skill(name: &str, path: &str, scope: SkillScope, plugin_id: Option<&str>) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: "description".to_string(),
            short_description: None,
            interface: None,
            dependencies: None,
            policy: None,
            path_to_skills_md: test_path_buf(path).abs(),
            scope,
            plugin_id: plugin_id.map(str::to_string),
        }
    }

    #[test]
    fn catalog_ids_are_deterministic_and_identity_sensitive() {
        let original = skill(
            "alpha",
            "/host/skills/alpha/SKILL.md",
            SkillScope::User,
            Some("plugin-a"),
        );
        let mut reordered_metadata = original.clone();
        reordered_metadata.name = "renamed without changing source identity".to_string();
        reordered_metadata.description = "different description".to_string();
        let id = skill_catalog_id(&original);

        assert_eq!(id, skill_catalog_id(&reordered_metadata));
        assert_eq!(id.len(), 24);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );

        let mut changed_scope = original.clone();
        changed_scope.scope = SkillScope::Repo;
        let mut changed_plugin = original.clone();
        changed_plugin.plugin_id = Some("plugin-b".to_string());
        let mut changed_locator = original;
        changed_locator.path_to_skills_md = test_path_buf("/host/skills/beta/SKILL.md").abs();
        assert_ne!(id, skill_catalog_id(&changed_scope));
        assert_ne!(id, skill_catalog_id(&changed_plugin));
        assert_ne!(id, skill_catalog_id(&changed_locator));
    }

    #[test]
    fn snapshot_resolves_catalog_locator_independent_of_input_order() {
        let alpha = skill(
            "alpha",
            "/host/skills/alpha/SKILL.md",
            SkillScope::Repo,
            None,
        );
        let beta = skill(
            "beta",
            "/host/skills/beta/SKILL.md",
            SkillScope::System,
            Some("plugin-b"),
        );
        let alpha_locator = format!("{SKILL_CATALOG_LOCATOR_PREFIX}{}", skill_catalog_id(&alpha));
        for skills in [vec![alpha.clone(), beta.clone()], vec![beta, alpha.clone()]] {
            let snapshot = HostSkillsSnapshot::new(Arc::new(SkillLoadOutcome {
                skills,
                ..Default::default()
            }));
            assert_eq!(
                snapshot.resolve_catalog_locator(&alpha_locator),
                Some(&alpha)
            );
        }
    }

    #[test]
    #[should_panic(expected = "duplicate deterministic skill catalog ID")]
    fn snapshot_rejects_duplicate_catalog_ids() {
        let first = skill(
            "alpha",
            "/host/skills/alpha/SKILL.md",
            SkillScope::Repo,
            None,
        );
        let mut duplicate = first.clone();
        duplicate.name = "duplicate".to_string();
        HostSkillsSnapshot::new(Arc::new(SkillLoadOutcome {
            skills: vec![first, duplicate],
            ..Default::default()
        }));
    }
}

#[derive(Clone, Default)]
pub(crate) struct SkillFileSystemsByPath {
    values: Arc<HashMap<AbsolutePathBuf, Arc<dyn ExecutorFileSystem>>>,
}

impl SkillFileSystemsByPath {
    pub(crate) fn new(values: HashMap<AbsolutePathBuf, Arc<dyn ExecutorFileSystem>>) -> Self {
        Self {
            values: Arc::new(values),
        }
    }

    fn get(&self, path: &AbsolutePathBuf) -> Option<Arc<dyn ExecutorFileSystem>> {
        self.values.get(path).map(Arc::clone)
    }

    fn retain_paths(&mut self, paths: &HashSet<AbsolutePathBuf>) {
        self.values = Arc::new(
            self.values
                .iter()
                .filter(|(path, _)| paths.contains(*path))
                .map(|(path, fs)| (path.clone(), Arc::clone(fs)))
                .collect(),
        );
    }
}

impl fmt::Debug for SkillFileSystemsByPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillFileSystemsByPath")
            .field("len", &self.values.len())
            .finish()
    }
}

pub fn filter_skill_load_outcome_for_product(
    mut outcome: SkillLoadOutcome,
    restriction_product: Option<Product>,
) -> SkillLoadOutcome {
    outcome
        .skills
        .retain(|skill| skill.matches_product_restriction_for_product(restriction_product));
    let retained_paths: HashSet<AbsolutePathBuf> = outcome
        .skills
        .iter()
        .map(|skill| skill.path_to_skills_md.clone())
        .collect();
    outcome
        .file_systems_by_skill_path
        .retain_paths(&retained_paths);
    outcome.skill_root_by_path = Arc::new(
        outcome
            .skill_root_by_path
            .iter()
            .filter(|(path, _)| retained_paths.contains(*path))
            .map(|(path, root)| (path.clone(), root.clone()))
            .collect(),
    );
    let retained_roots: HashSet<AbsolutePathBuf> =
        outcome.skill_root_by_path.values().cloned().collect();
    outcome
        .skill_roots
        .retain(|root| retained_roots.contains(root));
    outcome.implicit_skills_by_scripts_dir = Arc::new(
        outcome
            .implicit_skills_by_scripts_dir
            .iter()
            .filter(|(_, skill)| skill.matches_product_restriction_for_product(restriction_product))
            .map(|(path, skill)| (path.clone(), skill.clone()))
            .collect(),
    );
    outcome.implicit_skills_by_doc_path = Arc::new(
        outcome
            .implicit_skills_by_doc_path
            .iter()
            .filter(|(_, skill)| skill.matches_product_restriction_for_product(restriction_product))
            .map(|(path, skill)| (path.clone(), skill.clone()))
            .collect(),
    );
    outcome
}
