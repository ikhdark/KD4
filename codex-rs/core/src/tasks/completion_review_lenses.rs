use std::collections::BTreeSet;

use crate::task_evidence::CompletionReviewDossier;

use super::BEHAVIORAL_LENS;
use super::LIFECYCLE_LENS;
use super::PACKAGING_LENS;
use super::PERSISTENCE_LENS;
use super::PIPELINE_LENS;
use super::REVIEW_LENSES;
use super::SCHEMA_LENS;
use super::SECURITY_LENS;
use super::VALIDATION_LENS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReviewRiskDomain {
    Concurrency,
    Lifecycle,
    Persistence,
    Migration,
    Rollback,
    AtomicState,
    FilesystemSafety,
    Schema,
    Protocol,
    Security,
    Unsafe,
    Authentication,
    Permission,
    Sandbox,
    TrustBoundary,
    Installation,
    PlatformConfiguration,
    Manifest,
    Packaging,
    Installer,
    Publishing,
    Release,
    Ci,
    Cache,
    SnapshotProduction,
    Generator,
    ArtifactIdentity,
    Validation,
    TestOracle,
}

impl ReviewRiskDomain {
    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "concurrency" => Self::Concurrency,
            "lifecycle" => Self::Lifecycle,
            "persistence" => Self::Persistence,
            "migration" => Self::Migration,
            "rollback" => Self::Rollback,
            "atomic_state" | "atomic-state" => Self::AtomicState,
            "filesystem_safety" | "filesystem-safety" => Self::FilesystemSafety,
            "schema" => Self::Schema,
            "protocol" => Self::Protocol,
            "security" => Self::Security,
            "unsafe" => Self::Unsafe,
            "authentication" => Self::Authentication,
            "permission" | "permissions" => Self::Permission,
            "sandbox" => Self::Sandbox,
            "trust_boundary" | "trust-boundary" => Self::TrustBoundary,
            "installation" => Self::Installation,
            "platform_configuration" | "platform-configuration" => Self::PlatformConfiguration,
            "manifest" => Self::Manifest,
            "packaging" => Self::Packaging,
            "installer" => Self::Installer,
            "publishing" => Self::Publishing,
            "release" => Self::Release,
            "ci" => Self::Ci,
            "cache" => Self::Cache,
            "snapshot_production" | "snapshot-production" => Self::SnapshotProduction,
            "generator" => Self::Generator,
            "artifact_identity" | "artifact-identity" => Self::ArtifactIdentity,
            "validation" => Self::Validation,
            "test_oracle" | "test-oracle" => Self::TestOracle,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReviewSurfaceRole {
    Lifecycle,
    Persistence,
    Schema,
    Security,
    Packaging,
    Pipeline,
    Validation,
}

impl ReviewSurfaceRole {
    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "lifecycle" | "concurrency" => Self::Lifecycle,
            "persistence" | "migration" | "rollback" | "atomic_state" | "filesystem_safety" => {
                Self::Persistence
            }
            "schema" | "protocol" | "generated_representation" => Self::Schema,
            "security" | "unsafe" | "authentication" | "permission" | "sandbox"
            | "trust_boundary" => Self::Security,
            "installation"
            | "platform_configuration"
            | "manifest"
            | "packaging"
            | "installer"
            | "publishing"
            | "release" => Self::Packaging,
            "pipeline"
            | "ci"
            | "cache"
            | "snapshot_production"
            | "generator"
            | "artifact_identity" => Self::Pipeline,
            "validation" | "test" | "fixture" | "golden" | "snapshot" | "benchmark"
            | "validator" | "test_oracle" => Self::Validation,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ValidatedReviewPath(String);

impl ValidatedReviewPath {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let replaced = value.replace('\\', "/");
        if replaced.is_empty()
            || replaced.starts_with('/')
            || replaced.starts_with("//")
            || replaced.as_bytes().get(1) == Some(&b':')
        {
            return None;
        }
        let mut components = Vec::new();
        for component in replaced.split('/') {
            match component {
                "" | "." => {}
                ".." => return None,
                component => components.push(component.to_ascii_lowercase()),
            }
        }
        (!components.is_empty()).then(|| Self(components.join("/")))
    }
    fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
    fn basename(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReviewLensSelectionInput {
    pub(super) risk_domains: Vec<ReviewRiskDomain>,
    pub(super) hint_paths: Vec<ValidatedReviewPath>,
    pub(super) task_mutation_paths: Vec<ValidatedReviewPath>,
    pub(super) child_mutation_paths: Vec<ValidatedReviewPath>,
    pub(super) plan_edit_paths: Vec<ValidatedReviewPath>,
    pub(super) plan_runtime_paths: Vec<ValidatedReviewPath>,
    pub(super) surface_roles: Vec<ReviewSurfaceRole>,
    pub(super) validation_asset_paths: Vec<ValidatedReviewPath>,
    pub(super) generated_artifacts: Vec<ValidatedReviewPath>,
    pub(super) original_finding_lenses: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectedReviewLenses(Vec<&'static str>);

impl SelectedReviewLenses {
    pub(super) fn as_slice(&self) -> &[&'static str] {
        &self.0
    }
}

pub(super) fn select_review_lenses(input: &ReviewLensSelectionInput) -> SelectedReviewLenses {
    let mut selected = BTreeSet::from([BEHAVIORAL_LENS]);
    for domain in &input.risk_domains {
        selected.insert(match domain {
            ReviewRiskDomain::Concurrency | ReviewRiskDomain::Lifecycle => LIFECYCLE_LENS,
            ReviewRiskDomain::Persistence
            | ReviewRiskDomain::Migration
            | ReviewRiskDomain::Rollback
            | ReviewRiskDomain::AtomicState
            | ReviewRiskDomain::FilesystemSafety => PERSISTENCE_LENS,
            ReviewRiskDomain::Schema | ReviewRiskDomain::Protocol => SCHEMA_LENS,
            ReviewRiskDomain::Security
            | ReviewRiskDomain::Unsafe
            | ReviewRiskDomain::Authentication
            | ReviewRiskDomain::Permission
            | ReviewRiskDomain::Sandbox
            | ReviewRiskDomain::TrustBoundary => SECURITY_LENS,
            ReviewRiskDomain::Installation
            | ReviewRiskDomain::PlatformConfiguration
            | ReviewRiskDomain::Manifest
            | ReviewRiskDomain::Packaging
            | ReviewRiskDomain::Installer
            | ReviewRiskDomain::Publishing
            | ReviewRiskDomain::Release => PACKAGING_LENS,
            ReviewRiskDomain::Ci
            | ReviewRiskDomain::Cache
            | ReviewRiskDomain::SnapshotProduction
            | ReviewRiskDomain::Generator
            | ReviewRiskDomain::ArtifactIdentity => PIPELINE_LENS,
            ReviewRiskDomain::Validation | ReviewRiskDomain::TestOracle => VALIDATION_LENS,
        });
    }
    for role in &input.surface_roles {
        selected.insert(match role {
            ReviewSurfaceRole::Lifecycle => LIFECYCLE_LENS,
            ReviewSurfaceRole::Persistence => PERSISTENCE_LENS,
            ReviewSurfaceRole::Schema => SCHEMA_LENS,
            ReviewSurfaceRole::Security => SECURITY_LENS,
            ReviewSurfaceRole::Packaging => PACKAGING_LENS,
            ReviewSurfaceRole::Pipeline => PIPELINE_LENS,
            ReviewSurfaceRole::Validation => VALIDATION_LENS,
        });
    }
    if !input.validation_asset_paths.is_empty() {
        selected.insert(VALIDATION_LENS);
    }
    for path in input
        .hint_paths
        .iter()
        .chain(&input.task_mutation_paths)
        .chain(&input.child_mutation_paths)
        .chain(&input.plan_edit_paths)
        .chain(&input.plan_runtime_paths)
        .chain(&input.validation_asset_paths)
    {
        select_lenses_for_path(path, &mut selected);
    }
    if !input.generated_artifacts.is_empty() {
        selected.insert(SCHEMA_LENS);
        selected.insert(PIPELINE_LENS);
        for path in &input.generated_artifacts {
            select_lenses_for_path(path, &mut selected);
        }
    }
    for lens in &input.original_finding_lenses {
        if let Some(canonical) = REVIEW_LENSES.iter().find(|candidate| **candidate == lens) {
            selected.insert(*canonical);
        }
    }
    SelectedReviewLenses(
        REVIEW_LENSES
            .iter()
            .copied()
            .filter(|lens| selected.contains(lens))
            .collect(),
    )
}

fn select_lenses_for_path(path: &ValidatedReviewPath, selected: &mut BTreeSet<&'static str>) {
    let components = path.components().collect::<BTreeSet<_>>();
    let basename = path.basename();
    let extension = basename.rsplit_once('.').map(|(_, extension)| extension);
    if components
        .iter()
        .any(|c| matches!(*c, "lifecycle" | "concurrency" | "threads" | "async"))
    {
        selected.insert(LIFECYCLE_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "persistence" | "storage" | "migrations" | "rollback" | "filesystem"
        )
    }) || matches!(basename, "database.rs" | "storage.rs" | "migration.rs")
    {
        selected.insert(PERSISTENCE_LENS);
    }
    if components
        .iter()
        .any(|c| matches!(*c, "schema" | "schemas" | "protocol" | "generated"))
        || matches!(extension, Some("proto" | "graphql" | "jsonschema"))
    {
        selected.insert(SCHEMA_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "security" | "auth" | "authentication" | "permissions" | "sandbox" | "unsafe"
        )
    }) {
        selected.insert(SECURITY_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "packaging" | "installer" | "installers" | "release" | "publishing"
        )
    }) || matches!(
        basename,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "setup.py"
            | "requirements.txt"
            | "install.ps1"
            | "install.bat"
            | "installer.rs"
            | "dockerfile"
            | "manifest.json"
    ) {
        selected.insert(PACKAGING_LENS);
    }
    if components
        .iter()
        .any(|c| matches!(*c, "ci" | ".github" | "cache" | "snapshots" | "generators"))
        || matches!(
            basename,
            "cache.rs" | "cache.ts" | "generator.rs" | "generator.ts"
        )
    {
        selected.insert(PIPELINE_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "tests"
                | "test"
                | "fixtures"
                | "goldens"
                | "snapshots"
                | "benches"
                | "benchmarks"
                | "validators"
        )
    }) || matches!(extension, Some("snap"))
        || basename.ends_with("_test.rs")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".test.js")
    {
        selected.insert(VALIDATION_LENS);
    }
}

pub(super) fn build_review_lens_selection_input(
    dossier: &CompletionReviewDossier,
) -> Option<ReviewLensSelectionInput> {
    fn paths(values: &[String]) -> Option<Vec<ValidatedReviewPath>> {
        values
            .iter()
            .map(|value| ValidatedReviewPath::parse(value))
            .collect()
    }
    let facts = &dossier.review_lens_selection_facts;
    let mut risk_domains = Vec::new();
    let mut hint_paths = Vec::new();
    for hint in &facts.risk_hints {
        if let Some(domain) = ReviewRiskDomain::parse(hint) {
            risk_domains.push(domain);
        } else if let Some(path) = hint.strip_prefix("path:") {
            hint_paths.push(ValidatedReviewPath::parse(path)?);
        }
    }
    let surface_roles = facts
        .surface_roles
        .iter()
        .map(|role| ReviewSurfaceRole::parse(role))
        .collect::<Option<Vec<_>>>()?;
    if dossier
        .original_findings
        .iter()
        .any(|finding| !REVIEW_LENSES.contains(&finding.lens.as_str()))
    {
        return None;
    }
    Some(ReviewLensSelectionInput {
        risk_domains,
        hint_paths,
        task_mutation_paths: paths(&facts.task_mutation_paths)?,
        child_mutation_paths: paths(&facts.child_mutation_paths)?,
        plan_edit_paths: paths(&facts.plan_edit_paths)?,
        plan_runtime_paths: paths(&facts.plan_runtime_paths)?,
        surface_roles,
        validation_asset_paths: paths(&facts.validation_asset_paths)?,
        generated_artifacts: paths(&facts.generated_artifacts)?,
        original_finding_lenses: dossier
            .original_findings
            .iter()
            .map(|finding| finding.lens.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_surface_roles_are_all_consumable() {
        for role in [
            "lifecycle",
            "persistence",
            "schema",
            "security",
            "packaging",
            "pipeline",
            "validation",
        ] {
            assert!(ReviewSurfaceRole::parse(role).is_some(), "{role}");
        }
    }
}
