use crate::model::RawPath;
use globset::Glob;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageInfo {
    pub name: String,
    pub root: PathBuf,
    pub manifest: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CargoGraph {
    pub packages: BTreeMap<String, PackageInfo>,
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
    pub reverse_dependencies: BTreeMap<String, BTreeSet<String>>,
}

impl CargoGraph {
    pub fn package_for_path(&self, repo_root: &Path, path: &RawPath) -> Option<&PackageInfo> {
        let text = path.as_utf8()?;
        let absolute = repo_root.join(text.replace('/', std::path::MAIN_SEPARATOR_STR));
        self.packages
            .values()
            .filter(|package| absolute.starts_with(&package.root))
            .max_by_key(|package| package.root.components().count())
    }

    pub fn direct_reverse_dependencies(&self, packages: &[String]) -> Vec<String> {
        let mut adjacent = BTreeSet::new();
        for package in packages {
            if let Some(reverse) = self.reverse_dependencies.get(package) {
                adjacent.extend(reverse.iter().cloned());
            }
        }
        for package in packages {
            adjacent.remove(package);
        }
        adjacent.into_iter().collect()
    }

    #[allow(dead_code)]
    pub fn reverse_closure(&self, packages: &[String]) -> BTreeSet<String> {
        let mut result = BTreeSet::new();
        let mut pending = packages.to_vec();
        while let Some(package) = pending.pop() {
            if !result.insert(package.clone()) {
                continue;
            }
            if let Some(reverse) = self.reverse_dependencies.get(&package) {
                pending.extend(reverse.iter().cloned());
            }
        }
        result
    }

    pub fn has_cycle(&self) -> bool {
        fn visit(
            node: &str,
            graph: &CargoGraph,
            visiting: &mut BTreeSet<String>,
            visited: &mut BTreeSet<String>,
        ) -> bool {
            if visiting.contains(node) {
                return true;
            }
            if !visited.insert(node.to_string()) {
                return false;
            }
            visiting.insert(node.to_string());
            let cycle = graph.dependencies.get(node).is_some_and(|dependencies| {
                dependencies
                    .iter()
                    .any(|dependency| visit(dependency, graph, visiting, visited))
            });
            visiting.remove(node);
            cycle
        }

        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        self.packages
            .keys()
            .any(|package| visit(package, self, &mut visiting, &mut visited))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SurfaceRule {
    pub id: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub owned_packages: Vec<String>,
    pub test_expr: Option<String>,
    pub validation_command: Option<Vec<String>>,
    pub regen_command: Option<Vec<String>>,
    #[serde(default)]
    pub skip_owner_tests: bool,
    #[serde(default)]
    pub hash_paths: Vec<String>,
}

impl SurfaceRule {
    pub fn matches(&self, path: &str) -> bool {
        self.paths.iter().any(|pattern| {
            path == pattern
                || path.starts_with(&format!("{}/", pattern.trim_end_matches('/')))
                || Glob::new(pattern)
                    .ok()
                    .is_some_and(|glob| glob.compile_matcher().is_match(path))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannerContext {
    pub repository_root: PathBuf,
    pub workspace_root: PathBuf,
    pub graph: CargoGraph,
    pub rules: Vec<SurfaceRule>,
}

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("workspace member has no package name: {0}")]
    MissingPackageName(PathBuf),
    #[error("workspace root does not define workspace.members")]
    MissingWorkspaceMembers,
}

#[derive(Deserialize)]
struct RulesFile {
    #[serde(default)]
    surface: Vec<SurfaceRule>,
}

impl PlannerContext {
    pub fn load(repository_root: &Path) -> Result<Self, ContextError> {
        let repository_root =
            fs::canonicalize(repository_root).map_err(|source| ContextError::Read {
                path: repository_root.to_path_buf(),
                source,
            })?;
        let workspace_root = repository_root.join("codex-rs");
        let root_manifest = workspace_root.join("Cargo.toml");
        let root_value = read_toml(&root_manifest)?;
        let members = root_value
            .get("workspace")
            .and_then(|value| value.get("members"))
            .and_then(toml::Value::as_array)
            .ok_or(ContextError::MissingWorkspaceMembers)?;

        let mut graph = CargoGraph::default();
        for member in members.iter().filter_map(toml::Value::as_str) {
            if member.contains('*') || member.contains('?') || member.contains('[') {
                continue;
            }
            let root = workspace_root.join(member);
            let manifest = root.join("Cargo.toml");
            if !manifest.is_file() {
                continue;
            }
            let value = read_toml(&manifest)?;
            let name = value
                .get("package")
                .and_then(|value| value.get("name"))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| ContextError::MissingPackageName(manifest.clone()))?
                .to_string();
            graph.packages.insert(
                name.clone(),
                PackageInfo {
                    name,
                    root: fs::canonicalize(&root).unwrap_or(root),
                    manifest,
                },
            );
        }

        let workspace_names = graph.packages.keys().cloned().collect::<BTreeSet<_>>();
        let workspace_dependencies = root_value
            .get("workspace")
            .and_then(|value| value.get("dependencies"))
            .and_then(toml::Value::as_table)
            .map(|dependencies| {
                dependencies
                    .iter()
                    .map(|(alias, spec)| {
                        let name = spec
                            .as_table()
                            .and_then(|table| table.get("package"))
                            .and_then(toml::Value::as_str)
                            .unwrap_or(alias);
                        (alias.clone(), name.to_string())
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let package_manifests = graph
            .packages
            .values()
            .map(|package| (package.name.clone(), package.manifest.clone()))
            .collect::<Vec<_>>();
        for (package_name, manifest) in package_manifests {
            let value = read_toml(&manifest)?;
            let mut declared = BTreeSet::new();
            collect_dependencies(
                &value,
                &workspace_names,
                &workspace_dependencies,
                &mut declared,
            );
            for dependency in declared {
                graph
                    .dependencies
                    .entry(package_name.clone())
                    .or_default()
                    .insert(dependency.clone());
                graph
                    .reverse_dependencies
                    .entry(dependency)
                    .or_default()
                    .insert(package_name.clone());
            }
        }

        let rules_path = repository_root.join("scripts/verify_local_rules.toml");
        let rules = if rules_path.is_file() {
            let text = fs::read_to_string(&rules_path).map_err(|source| ContextError::Read {
                path: rules_path.clone(),
                source,
            })?;
            toml::from_str::<RulesFile>(&text)
                .map_err(|source| ContextError::Parse {
                    path: rules_path,
                    source,
                })?
                .surface
        } else {
            Vec::new()
        };

        Ok(Self {
            repository_root,
            workspace_root,
            graph,
            rules,
        })
    }

    pub fn matching_rules<'a>(&'a self, paths: &[RawPath]) -> Vec<&'a SurfaceRule> {
        self.rules
            .iter()
            .filter(|rule| {
                paths
                    .iter()
                    .filter_map(RawPath::as_utf8)
                    .any(|path| rule.matches(path))
            })
            .collect()
    }

    pub fn owner_packages(&self, paths: &[RawPath]) -> Vec<String> {
        let mut packages = BTreeSet::new();
        for rule in self.matching_rules(paths) {
            packages.extend(rule.owned_packages.iter().cloned());
        }
        for path in paths {
            if let Some(package) = self.graph.package_for_path(&self.repository_root, path) {
                packages.insert(package.name.clone());
            }
        }
        packages.into_iter().collect()
    }
}

fn read_toml(path: &Path) -> Result<toml::Value, ContextError> {
    let text = fs::read_to_string(path).map_err(|source| ContextError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ContextError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn collect_dependencies(
    value: &toml::Value,
    workspace_names: &BTreeSet<String>,
    workspace_dependencies: &BTreeMap<String, String>,
    output: &mut BTreeSet<String>,
) {
    if let Some(table) = value.as_table() {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = table.get(key).and_then(toml::Value::as_table) {
                for (alias, spec) in dependencies {
                    let inherited = spec
                        .as_table()
                        .and_then(|table| table.get("workspace"))
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false);
                    let name = spec
                        .as_table()
                        .and_then(|table| table.get("package"))
                        .and_then(toml::Value::as_str)
                        .or_else(|| {
                            inherited
                                .then(|| workspace_dependencies.get(alias).map(String::as_str))
                                .flatten()
                        })
                        .unwrap_or(alias);
                    if workspace_names.contains(name) {
                        output.insert(name.to_string());
                    }
                }
            }
        }
        if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
            for target in targets.values() {
                collect_dependencies(target, workspace_names, workspace_dependencies, output);
            }
        }
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
