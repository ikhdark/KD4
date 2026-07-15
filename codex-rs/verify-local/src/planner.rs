use crate::context::PlannerContext;
use crate::context::SurfaceRule;
use crate::model::CommandArgV2;
use crate::model::CommandSpecV2;
use crate::model::DirtyGroup;
use crate::model::PlanEnvelopeV2;
use crate::model::PlanMode;
use crate::model::PlanRequest;
use crate::model::RawPath;
use crate::model::RepositorySnapshot;
use crate::model::ScopeV2;
use crate::model::SkippedDecision;
use crate::model::SnapshotSource;
use crate::model::Verdict;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

const OWNER_TIMEOUT_MS: u64 = 20 * 60 * 1_000;
const SCRIPT_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const FORMATTER_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
const HYGIENE_TIMEOUT_MS: u64 = 60 * 1_000;
const SCHEMA_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

pub fn plan_verification(request: PlanRequest, snapshot: RepositorySnapshot) -> PlanEnvelopeV2 {
    let mode = request.mode.unwrap_or(PlanMode::Plan);
    let invocation_id = invocation_id(&request, &snapshot);
    let mut plan = PlanEnvelopeV2::new(mode, invocation_id);
    plan.enabled_expansions = enabled_expansions(&request);
    if !snapshot.complete {
        plan.verdict = Some(Verdict::Inconclusive);
        plan.skipped.extend(
            snapshot
                .fallback_reasons
                .iter()
                .map(|reason| SkippedDecision {
                    item: "repository snapshot".to_string(),
                    reason: reason.clone(),
                }),
        );
        return plan;
    }
    let Some(repository_root) = snapshot.repository_root.as_deref() else {
        plan.verdict = Some(Verdict::ToolingError);
        plan.skipped.push(SkippedDecision {
            item: "planner context".to_string(),
            reason: "repository root was not supplied".to_string(),
        });
        return plan;
    };
    let context = match PlannerContext::load(repository_root) {
        Ok(context) => context,
        Err(error) => {
            plan.verdict = Some(Verdict::ToolingError);
            plan.skipped.push(SkippedDecision {
                item: "planner context".to_string(),
                reason: error.to_string(),
            });
            return plan;
        }
    };

    let scope = build_scope(&request, &snapshot, &context);
    plan.scope = Some(scope.clone());
    if !scope.stale_reasons.is_empty() {
        plan.verdict = Some(Verdict::Inconclusive);
        return plan;
    }
    if scope.source == "dirty-groups" {
        plan.verdict = Some(Verdict::NeedsScope);
        return plan;
    }
    if scope.source == "scope-reset" || scope.active_files.is_empty() {
        plan.verdict = Some(Verdict::VerifiedNoProof);
        return plan;
    }

    plan.skipped.push(SkippedDecision {
        item: "workspace tests".to_string(),
        reason: "blocked unless --allow-workspace is set".to_string(),
    });
    if !scope.adjacent_packages.is_empty() && !request.related_tests {
        plan.skipped.push(SkippedDecision {
            item: "related tests".to_string(),
            reason: "blocked unless --related-tests is set".to_string(),
        });
    }
    plan.skipped.push(SkippedDecision {
        item: "just fmt".to_string(),
        reason: "mutating formatter is not verification".to_string(),
    });
    if !scope.adjacent_packages.is_empty() {
        plan.skipped.push(SkippedDecision {
            item: scope.adjacent_packages.join(", "),
            reason: "adjacent packages are compile-check only and require --related or --allow-workspace"
                .to_string(),
        });
    }

    let matched_rules = context.matching_rules(&scope.active_files);
    let mut commands = surface_commands(&request, &matched_rules, &context);
    let suppressed = matched_rules
        .iter()
        .filter(|rule| rule.skip_owner_tests)
        .flat_map(|rule| rule.owned_packages.iter().cloned())
        .collect::<BTreeSet<_>>();
    if !suppressed.is_empty() {
        plan.skipped.push(SkippedDecision {
            item: suppressed.iter().cloned().collect::<Vec<_>>().join(", "),
            reason: "surface rule owns focused validation".to_string(),
        });
    }
    let owner_packages = scope
        .owned_packages
        .iter()
        .filter(|package| !suppressed.contains(*package))
        .cloned()
        .collect::<Vec<_>>();
    commands.extend(owner_commands(
        &request,
        &scope,
        &matched_rules,
        &owner_packages,
        &context,
    ));
    commands.extend(script_commands(&scope, &context));
    if (request.related || request.allow_workspace) && !scope.adjacent_packages.is_empty() {
        commands.extend(adjacent_commands(&scope.adjacent_packages, &context, false));
    }
    if request.related_tests && !scope.adjacent_packages.is_empty() {
        commands.extend(adjacent_commands(&scope.adjacent_packages, &context, true));
    }
    if mode == PlanMode::Final {
        let mut formatter = formatter_commands(&scope, &context);
        formatter.extend(commands);
        commands = formatter;
    }
    if scope
        .active_files
        .iter()
        .any(|path| path.as_utf8() == Some("justfile"))
    {
        commands.push(command(
            "justfile:summary",
            "justfile_check",
            vec![text("just"), text("--summary")],
            HYGIENE_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "parse justfile recipes",
            &context,
        ));
    }
    commands.push(hygiene_command(&scope, &context));
    plan.commands = commands;
    plan
}

fn build_scope(
    request: &PlanRequest,
    snapshot: &RepositorySnapshot,
    context: &PlannerContext,
) -> ScopeV2 {
    if request.scope_reset {
        let _ = fs::remove_file(scope_state_path(&context.repository_root));
        return ScopeV2 {
            scope_id: "scope-reset".to_string(),
            source: "scope-reset".to_string(),
            ..ScopeV2::default()
        };
    }
    let all_files = stable_paths(snapshot.records.iter().map(|record| record.path.clone()));
    let staged_files = stable_paths(
        snapshot
            .records
            .iter()
            .filter(|record| record.staged)
            .map(|record| record.path.clone()),
    );
    let (source, active_files) = if let Some(scope_name) = request.scope_start.as_deref() {
        let active = stable_paths(request.changed.clone());
        if let Err(error) = write_sticky_scope(&context.repository_root, scope_name, &active) {
            return ScopeV2 {
                scope_id: scope_name.to_string(),
                source: "scope-start".to_string(),
                stale_reasons: vec![error],
                ..ScopeV2::default()
            };
        }
        ("scope-start", active)
    } else if !request.scope_add.is_empty() {
        match read_sticky_scope(&context.repository_root) {
            Ok((scope_id, mut active)) => {
                active.extend(request.scope_add.clone());
                let active = stable_paths(active);
                if let Err(error) = write_sticky_scope(&context.repository_root, &scope_id, &active)
                {
                    return ScopeV2 {
                        scope_id,
                        source: "scope-add".to_string(),
                        stale_reasons: vec![error],
                        ..ScopeV2::default()
                    };
                }
                ("scope-add", active)
            }
            Err(error) => {
                return ScopeV2 {
                    scope_id: "current".to_string(),
                    source: "scope-add".to_string(),
                    stale_reasons: vec![error],
                    ..ScopeV2::default()
                };
            }
        }
    } else if request.scope_current {
        match read_sticky_scope(&context.repository_root) {
            Ok((_scope_id, active)) => ("scope-current", active),
            Err(error) => {
                return ScopeV2 {
                    scope_id: "current".to_string(),
                    source: "scope-current".to_string(),
                    stale_reasons: vec![error],
                    ..ScopeV2::default()
                };
            }
        }
    } else if !request.changed.is_empty() {
        ("changed", stable_paths(request.changed.clone()))
    } else if request.staged {
        ("staged", staged_files)
    } else if request.all_dirty {
        ("all-dirty", all_files.clone())
    } else if !staged_files.is_empty() {
        ("staged", staged_files)
    } else {
        match snapshot.source {
            SnapshotSource::ExplicitPaths => ("changed", all_files.clone()),
            SnapshotSource::CommitDiff { .. } => ("commit-diff", all_files.clone()),
            SnapshotSource::Worktree => {
                let groups = dirty_groups(&all_files, context);
                if groups.len() > 1 {
                    return ScopeV2 {
                        scope_id: "needs-scope".to_string(),
                        source: "dirty-groups".to_string(),
                        dirty_groups: groups,
                        ..ScopeV2::default()
                    };
                }
                if let Some(group) = groups.first() {
                    ("single-dirty-group", group.paths.clone())
                } else {
                    ("empty", Vec::new())
                }
            }
        }
    };
    let owned_packages = context.owner_packages(&active_files);
    let active = active_files.iter().cloned().collect::<BTreeSet<_>>();
    let ignored_dirty_files = all_files
        .iter()
        .filter(|path| !active.contains(*path) && !is_ignored_output(path))
        .cloned()
        .collect();
    let adjacent_packages = context.graph.direct_reverse_dependencies(&owned_packages);
    let surface_rules = context
        .matching_rules(&active_files)
        .into_iter()
        .map(|rule| rule.id.clone())
        .collect();
    ScopeV2 {
        scope_id: scope_id(&active_files),
        source: source.to_string(),
        active_files,
        owned_packages,
        ignored_dirty_files,
        adjacent_packages,
        stale_reasons: Vec::new(),
        dirty_groups: Vec::new(),
        surface_rules,
    }
}

fn dirty_groups(paths: &[RawPath], context: &PlannerContext) -> Vec<DirtyGroup> {
    let mut groups = BTreeMap::<String, Vec<RawPath>>::new();
    for path in paths.iter().filter(|path| !is_ignored_output(path)) {
        let key = classify_group(path, context);
        groups.entry(key).or_default().push(path.clone());
    }
    groups
        .into_iter()
        .map(|(id, paths)| DirtyGroup { id, paths })
        .collect()
}

fn classify_group(path: &RawPath, context: &PlannerContext) -> String {
    if let Some(rule) = context.matching_rules(std::slice::from_ref(path)).first() {
        return format!("contract:{}", rule.id);
    }
    let text = path.as_utf8().unwrap_or_default();
    if matches!(
        text,
        "Cargo.toml" | "Cargo.lock" | "rust-toolchain.toml" | "justfile"
    ) {
        return format!("contract:{text}");
    }
    if let Some(package) = context
        .graph
        .package_for_path(&context.repository_root, path)
    {
        return format!("package:{}", package.name);
    }
    format!("area:{}", text.split('/').next().unwrap_or("root"))
}

fn owner_commands(
    request: &PlanRequest,
    scope: &ScopeV2,
    rules: &[&SurfaceRule],
    packages: &[String],
    context: &PlannerContext,
) -> Vec<CommandSpecV2> {
    if packages.is_empty() {
        return Vec::new();
    }
    let test_exprs = rules
        .iter()
        .filter_map(|rule| rule.test_expr.as_deref())
        .map(str::trim)
        .filter(|expr| !expr.is_empty())
        .collect::<Vec<_>>();
    let scope_has_tests = scope
        .active_files
        .iter()
        .filter_map(RawPath::as_utf8)
        .any(|path| {
            let stem = Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            stem.contains("_test") || stem == "tests" || path.contains("/tests/")
        });
    packages
        .iter()
        .map(|package| {
            if request.mode == Some(PlanMode::Fast)
                && !request.isolated
                && test_exprs.is_empty()
                && !scope_has_tests
            {
                return command(
                    format!("owner-check:{package}"),
                    "owner_check",
                    vec![text("just"), text("check-lane"), text(package)],
                    OWNER_TIMEOUT_MS,
                    vec![package.clone()],
                    Vec::new(),
                    "fast owner package compile proof",
                    context,
                );
            }
            if request.isolated {
                return command(
                    format!("owner-test:{package}"),
                    "owner_test",
                    vec![text("just"), text("test-lane-package"), text(package)],
                    OWNER_TIMEOUT_MS,
                    vec![package.clone()],
                    Vec::new(),
                    "isolated owner package proof",
                    context,
                );
            }
            let mut args = vec![text("just"), text("test-fast"), text("-p"), text(package)];
            if packages.len() == 1 && !test_exprs.is_empty() {
                args.push(text("-E"));
                args.push(text(
                    test_exprs
                        .iter()
                        .map(|expr| format!("({expr})"))
                        .collect::<Vec<_>>()
                        .join(" | "),
                ));
            }
            command(
                format!("owner-test:{package}"),
                "owner_test",
                args,
                OWNER_TIMEOUT_MS,
                vec![package.clone()],
                Vec::new(),
                "owner package proof",
                context,
            )
        })
        .collect()
}

fn surface_commands(
    request: &PlanRequest,
    rules: &[&SurfaceRule],
    context: &PlannerContext,
) -> Vec<CommandSpecV2> {
    rules
        .iter()
        .filter_map(|rule| {
            let regenerating = request.regen && rule.regen_command.is_some();
            let args = if regenerating {
                rule.regen_command.as_ref()
            } else {
                rule.validation_command.as_ref()
            }?;
            let mut hash_paths = rule.paths.clone();
            hash_paths.extend(rule.hash_paths.clone());
            hash_paths.sort();
            hash_paths.dedup();
            Some(command(
                format!(
                    "surface:{}:{}",
                    rule.id,
                    if regenerating { "regen" } else { "validate" }
                ),
                if regenerating {
                    "surface_regen"
                } else {
                    "surface_validation"
                },
                args.iter().map(text).collect(),
                SCHEMA_TIMEOUT_MS,
                rule.owned_packages.clone(),
                hash_paths.into_iter().map(RawPath::from_utf8).collect(),
                format!(
                    "{} surface {}",
                    rule.id,
                    if regenerating {
                        "regeneration and validation"
                    } else {
                        "validation"
                    }
                ),
                context,
            ))
        })
        .collect()
}

fn script_commands(scope: &ScopeV2, context: &PlannerContext) -> Vec<CommandSpecV2> {
    let python = scope
        .active_files
        .iter()
        .filter(|path| {
            path.as_utf8()
                .is_some_and(|path| path.starts_with("scripts/") && path.ends_with(".py"))
        })
        .cloned()
        .collect::<Vec<_>>();
    let powershell = scope
        .active_files
        .iter()
        .filter(|path| {
            path.as_utf8()
                .is_some_and(|path| path.starts_with("scripts/") && path.ends_with(".ps1"))
        })
        .cloned()
        .collect::<Vec<_>>();
    let shell = scope
        .active_files
        .iter()
        .filter(|path| {
            path.as_utf8()
                .is_some_and(|path| path.starts_with("scripts/") && path.ends_with(".sh"))
        })
        .cloned()
        .collect::<Vec<_>>();
    let controls = scope.active_files.iter().any(|path| {
        matches!(
            path.as_utf8(),
            Some("scripts/verify_local.py")
                | Some("scripts/verify_local_context.py")
                | Some("scripts/verify_local_execution.py")
                | Some("scripts/verify_local_rules.toml")
                | Some("scripts/test_verify_local.py")
                | Some("justfile")
        )
    });
    let mut commands = Vec::new();
    for path in powershell {
        commands.push(command(
            format!("script-syntax:powershell:{}", path_id(&path)),
            "script_syntax",
            vec![
                text("pwsh"),
                text("-NoProfile"),
                text("-Command"),
                text("[void][System.Management.Automation.Language.Parser]::ParseFile($args[0],[ref]$null,[ref]$errors); if ($errors.Count) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }"),
                CommandArgV2::path(path),
            ],
            SCRIPT_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "PowerShell script parse check",
            context,
        ));
    }
    for path in shell {
        commands.push(command(
            format!("script-syntax:shell:{}", path_id(&path)),
            "script_syntax",
            vec![text("bash"), text("-n"), CommandArgV2::path(path)],
            SCRIPT_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "shell script parse check",
            context,
        ));
    }
    if !python.is_empty() {
        let mut args = root_maintenance_args("lint-python");
        for path in &python {
            args.push(text("--changed"));
            args.push(CommandArgV2::path(path.clone()));
        }
        commands.push(command(
            format!(
                "script-lint:{}",
                python.iter().map(path_id).collect::<Vec<_>>().join("+")
            ),
            "script_lint",
            args,
            SCRIPT_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "scoped Python script lint check",
            context,
        ));
    }
    if controls {
        let mut args = root_maintenance_args("test-python");
        args.extend([text("--module"), text("scripts.test_verify_local")]);
        commands.push(command(
            "script-test:verify_local_controls",
            "script_test",
            args,
            SCRIPT_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "nearest verify-local router tests",
            context,
        ));
    }
    commands
}

fn formatter_commands(scope: &ScopeV2, context: &PlannerContext) -> Vec<CommandSpecV2> {
    let mut commands = Vec::new();
    let paths = scope
        .active_files
        .iter()
        .filter_map(RawPath::as_utf8)
        .collect::<Vec<_>>();
    if paths
        .iter()
        .any(|path| path.starts_with("codex-rs/") || *path == "justfile")
    {
        commands.push(command(
            "formatter:fmt-check-fast",
            "formatter",
            vec![text("just"), text("fmt-check-fast")],
            FORMATTER_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "check-only fast Rust/just formatter",
            context,
        ));
    }
    if paths
        .iter()
        .any(|path| path.starts_with("scripts/") && path.ends_with(".py"))
    {
        let mut args = root_maintenance_args("format-python");
        for path in &scope.active_files {
            if path
                .as_utf8()
                .is_some_and(|path| path.starts_with("scripts/") && path.ends_with(".py"))
            {
                args.push(text("--changed"));
                args.push(CommandArgV2::path(path.clone()));
            }
        }
        commands.push(command(
            "formatter:format-python",
            "formatter",
            args,
            FORMATTER_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "check-only Python script formatter",
            context,
        ));
    }
    if paths.iter().any(|path| needs_prettier(path)) {
        commands.push(command(
            "formatter:format-prettier",
            "formatter",
            root_maintenance_args("format-prettier"),
            FORMATTER_TIMEOUT_MS,
            Vec::new(),
            Vec::new(),
            "check-only Prettier formatter",
            context,
        ));
    }
    commands
}

fn adjacent_commands(
    packages: &[String],
    context: &PlannerContext,
    tests: bool,
) -> Vec<CommandSpecV2> {
    packages
        .iter()
        .take(3)
        .map(|package| {
            command(
                format!(
                    "{}:{package}",
                    if tests { "related-test" } else { "owner-check" }
                ),
                if tests { "related_test" } else { "owner_check" },
                vec![
                    text("just"),
                    text(if tests {
                        "test-lane-package"
                    } else {
                        "check-lane"
                    }),
                    text(package),
                ],
                OWNER_TIMEOUT_MS,
                vec![package.clone()],
                Vec::new(),
                if tests {
                    "explicit related package test proof"
                } else {
                    "explicit adjacent compile check"
                },
                context,
            )
        })
        .collect()
}

fn hygiene_command(scope: &ScopeV2, context: &PlannerContext) -> CommandSpecV2 {
    let mut args = vec![
        text("git"),
        text("diff"),
        text("--check"),
        text("HEAD"),
        text("--"),
    ];
    args.extend(scope.active_files.iter().cloned().map(CommandArgV2::path));
    command(
        "hygiene:diff-check",
        "hygiene",
        args,
        HYGIENE_TIMEOUT_MS,
        Vec::new(),
        Vec::new(),
        "scoped diff whitespace check",
        context,
    )
}

fn command(
    id: impl Into<String>,
    kind: impl Into<String>,
    args: Vec<CommandArgV2>,
    timeout_ms: u64,
    owner_packages: Vec<String>,
    hash_paths: Vec<RawPath>,
    reason: impl Into<String>,
    context: &PlannerContext,
) -> CommandSpecV2 {
    CommandSpecV2 {
        id: id.into(),
        kind: kind.into(),
        args,
        cwd: RawPath::from_utf8(context.repository_root.to_string_lossy()),
        timeout_ms,
        owner_packages,
        hash_paths,
        reason: reason.into(),
    }
}

fn root_maintenance_args(operation: &str) -> Vec<CommandArgV2> {
    vec![
        text(std::env::var("CODEX_VERIFY_LOCAL_PYTHON").unwrap_or_else(|_| "python".to_string())),
        text("-m"),
        text("scripts.root_maintenance"),
        text(operation),
    ]
}

fn text(value: impl Into<String>) -> CommandArgV2 {
    CommandArgV2::text(value)
}

fn enabled_expansions(request: &PlanRequest) -> Vec<String> {
    [
        (request.related, "--related"),
        (request.related_tests, "--related-tests"),
        (request.allow_workspace, "--allow-workspace"),
        (request.all_dirty, "--all-dirty"),
        (request.isolated, "--isolated"),
        (request.regen, "--regen"),
        (request.baseline, "--baseline"),
        (request.no_cache, "--no-cache"),
        (request.cache_readonly, "--cache-readonly"),
    ]
    .into_iter()
    .filter_map(|(enabled, flag)| enabled.then(|| flag.to_string()))
    .collect()
}

fn is_ignored_output(path: &RawPath) -> bool {
    path.as_utf8().is_some_and(|path| {
        path.split('/')
            .any(|part| matches!(part, "target" | "node_modules" | ".venv" | "__pycache__"))
    })
}

fn needs_prettier(path: &str) -> bool {
    matches!(
        path,
        "package.json" | "knip.json" | "pnpm-workspace.yaml" | "eslint.config.mjs"
    ) || (path.starts_with("docs/") && path.ends_with(".md"))
        || (path.starts_with(".github/workflows/") && path.ends_with(".yml"))
        || (path.starts_with("codex-cli/") && path.ends_with(".js"))
        || (path.starts_with("sdk/typescript/") && (path.ends_with(".js") || path.ends_with(".ts")))
}

fn stable_paths(paths: impl IntoIterator<Item = RawPath>) -> Vec<RawPath> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn scope_id(paths: &[RawPath]) -> String {
    if paths.is_empty() {
        return "empty".to_string();
    }
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update([b'\n']);
    }
    let digest = format!("{:x}", hasher.finalize());
    let first = paths[0]
        .as_utf8()
        .and_then(|path| Path::new(path).file_stem())
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("scope");
    format!("{first}-{}", &digest[..10])
}

fn path_id(path: &RawPath) -> String {
    let digest = Sha256::digest(path.as_bytes());
    format!("{:x}", digest)[..12].to_string()
}

fn invocation_id(request: &PlanRequest, snapshot: &RepositorySnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.mode.unwrap_or(PlanMode::Plan).as_str().as_bytes());
    for record in &snapshot.records {
        hasher.update(record.status.as_bytes());
        hasher.update([0]);
        hasher.update(record.path.as_bytes());
        hasher.update([0xff]);
        if let Some(original) = &record.original_path {
            hasher.update(original.as_bytes());
        }
        hasher.update([0xfe]);
    }
    format!("{:x}", hasher.finalize())
}

fn scope_state_path(repository_root: &Path) -> std::path::PathBuf {
    repository_root.join(".codex/verify-local/scope.json")
}

fn read_sticky_scope(repository_root: &Path) -> Result<(String, Vec<RawPath>), String> {
    let path = scope_state_path(repository_root);
    let bytes = fs::read(&path).map_err(|error| format!("no sticky scope exists: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("sticky scope is malformed: {error}"))?;
    let scope_id = value
        .get("scope_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("current")
        .to_string();
    let paths = value
        .get("owned_paths")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "sticky scope has no owned_paths".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(RawPath::from_utf8)
                .ok_or_else(|| "sticky scope path is not text".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((scope_id, paths))
}

fn write_sticky_scope(
    repository_root: &Path,
    scope_id: &str,
    paths: &[RawPath],
) -> Result<(), String> {
    let destination = scope_state_path(repository_root);
    let parent = destination
        .parent()
        .ok_or_else(|| "scope path has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create scope directory: {error}"))?;
    let text_paths = paths
        .iter()
        .map(|path| {
            path.as_utf8()
                .map(str::to_string)
                .ok_or_else(|| "sticky V1 scope cannot contain non-UTF-8 paths".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "scope_id": scope_id,
        "owned_paths": text_paths,
    }))
    .map_err(|error| format!("failed to serialize sticky scope: {error}"))?;
    let temporary = destination.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("failed to create scope temporary file: {error}"))?;
    file.write_all(&body)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("failed to persist scope temporary file: {error}"))?;
    drop(file);
    if destination.exists() {
        fs::remove_file(&destination)
            .map_err(|error| format!("failed to replace sticky scope: {error}"))?;
    }
    fs::rename(&temporary, &destination)
        .map_err(|error| format!("failed to install sticky scope: {error}"))
}
