use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use sha1::digest::Output;

use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DEV_NULL: &str = "/dev/null";
const REGULAR_FILE_MODE: &str = "100644";
// Normal edits finish well within 100 ms; pathological inputs fall back to a coarse,
// content-exact diff without stalling tool completion.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

struct TrackedContent {
    content: String,
    revision: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TrackedPath {
    environment_id: String,
    path: PathBuf,
}

impl TrackedPath {
    fn new(environment_id: &str, path: &Path) -> Self {
        Self {
            environment_id: environment_id.to_string(),
            path: path.to_path_buf(),
        }
    }
}

#[derive(Eq, Hash, PartialEq)]
struct DiffCacheKey {
    left_path: TrackedPath,
    left_revision: Option<u64>,
    right_path: TrackedPath,
    right_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ValidationFreshnessStatus {
    None,
    StaleAfterLastMutation,
    FormatOnly,
    AdvisoryBroadFilter,
    ScopedValidationIncomplete,
    PassedAfterLastMutation,
    FailedAfterLastMutation,
    TimedOut,
}

impl ValidationFreshnessStatus {
    pub(crate) fn final_warning_message(&self) -> Option<&'static str> {
        match self {
            Self::PassedAfterLastMutation => None,
            Self::None => Some(
                "Changed files have not been followed by post-change correctness validation. Wiring evidence is separate from tests, builds, and lint; final answers should state what passed, failed, or was skipped.",
            ),
            Self::StaleAfterLastMutation => Some(
                "Changed files were modified after the last successful correctness validation, so that evidence is stale.",
            ),
            Self::FormatOnly => Some(
                "Changed files have only been followed by formatting. Formatting does not prove correctness or runtime reachability.",
            ),
            Self::AdvisoryBroadFilter => Some(
                "Changed files have only been followed by a broad validation filter. Broad filters are advisory and do not prove focused correctness by themselves.",
            ),
            Self::ScopedValidationIncomplete => Some(
                "Successful validation covered only part of the changed-file scope; remaining changed files still need correctness validation.",
            ),
            Self::FailedAfterLastMutation => {
                Some("Changed files were followed by correctness validation that failed.")
            }
            Self::TimedOut => {
                Some("Changed files were followed by correctness validation that timed out.")
            }
        }
    }
}

/// Tracks the net text diff for the current turn from committed apply_patch
/// mutations, without rereading the workspace filesystem.
pub struct TurnDiffTracker {
    valid: bool,
    display_roots_by_environment: HashMap<String, PathBuf>,
    baseline_by_path: HashMap<TrackedPath, TrackedContent>,
    current_by_path: HashMap<TrackedPath, TrackedContent>,
    origin_by_current_path: HashMap<TrackedPath, TrackedPath>,
    next_revision: u64,
    rendered_diffs: HashMap<DiffCacheKey, Option<String>>,
    unified_diff: Option<String>,
    unvalidated_paths: HashSet<TrackedPath>,
    unvalidated_unknown_mutation: bool,
    has_successful_validation: bool,
    last_post_mutation_validation_status: ValidationFreshnessStatus,
    #[cfg(test)]
    rendered_diff_count: std::cell::Cell<usize>,
}

impl Default for TurnDiffTracker {
    fn default() -> Self {
        Self {
            valid: true,
            display_roots_by_environment: HashMap::new(),
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            origin_by_current_path: HashMap::new(),
            next_revision: 0,
            rendered_diffs: HashMap::new(),
            unified_diff: None,
            unvalidated_paths: HashSet::new(),
            unvalidated_unknown_mutation: false,
            has_successful_validation: false,
            last_post_mutation_validation_status: ValidationFreshnessStatus::None,
            #[cfg(test)]
            rendered_diff_count: std::cell::Cell::new(0),
        }
    }
}

impl TurnDiffTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_environment_display_roots(
        display_roots: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> Self {
        let mut tracker = Self::new();
        tracker.display_roots_by_environment = display_roots.into_iter().collect();
        tracker
    }

    pub fn track_delta(&mut self, environment_id: &str, delta: &AppliedPatchDelta) {
        if !delta.is_empty() {
            self.record_mutation(paths_touched_by_delta(environment_id, delta));
        }

        if !self.valid {
            return;
        }

        if !delta.is_exact() {
            self.invalidate();
            return;
        }

        for change in delta.changes() {
            self.apply_change(environment_id, change);
        }
        self.refresh_unified_diff();
    }

    pub fn invalidate(&mut self) {
        self.valid = false;
        self.rendered_diffs.clear();
        self.unified_diff = None;
    }

    pub(crate) fn record_unknown_mutation(&mut self) {
        self.record_mutation(HashSet::new());
        self.invalidate();
    }

    pub(crate) fn record_exec_command_end_at(
        &mut self,
        command: &[String],
        exit_code: i32,
        timed_out: bool,
        cwd: Option<&Path>,
    ) {
        let was_post_mutation = self.has_unvalidated_mutation();
        let is_validation = is_validation_command(command);
        let format_only = is_format_only_command(command);
        let broad_filter = is_broad_validation_filter_command(command);
        let possible_mutation = looks_like_mutating_command(command);

        // A command can write before failing or timing out, so known mutators
        // always invalidate exact diff/freshness state.
        if possible_mutation {
            self.record_unknown_mutation();
        }

        let is_post_mutation = was_post_mutation || self.has_unvalidated_mutation();

        if is_post_mutation {
            self.last_post_mutation_validation_status = if format_only {
                ValidationFreshnessStatus::FormatOnly
            } else if timed_out && is_validation {
                ValidationFreshnessStatus::TimedOut
            } else if is_validation && exit_code == 0 && broad_filter {
                ValidationFreshnessStatus::AdvisoryBroadFilter
            } else if is_validation && exit_code == 0 && possible_mutation {
                ValidationFreshnessStatus::StaleAfterLastMutation
            } else if is_validation {
                ValidationFreshnessStatus::FailedAfterLastMutation
            } else {
                self.last_post_mutation_validation_status.clone()
            };
        }

        if is_validation && exit_code == 0 && !timed_out && !broad_filter && !possible_mutation {
            self.has_successful_validation = true;
            match validation_coverage(command, cwd) {
                ValidationCoverage::All => {
                    self.unvalidated_paths.clear();
                    self.unvalidated_unknown_mutation = false;
                }
                ValidationCoverage::Paths(paths) => self.clear_covered_paths(&paths),
                ValidationCoverage::ScopedUnknown => {}
            }
            self.last_post_mutation_validation_status = if self.has_unvalidated_mutation() {
                ValidationFreshnessStatus::ScopedValidationIncomplete
            } else {
                ValidationFreshnessStatus::PassedAfterLastMutation
            };
        }
    }

    pub(crate) fn record_verified_validation(
        &mut self,
        _command: Vec<String>,
        environment_id: &str,
        active_files: &[PathBuf],
        clear_unknown_mutation: bool,
    ) -> bool {
        let covered_candidates = active_files
            .iter()
            .flat_map(|path| {
                let mut candidates = vec![TrackedPath::new(environment_id, path)];
                if path.is_relative()
                    && let Some(root) = self.display_roots_by_environment.get(environment_id)
                {
                    candidates.push(TrackedPath::new(environment_id, root.join(path).as_path()));
                }
                candidates
            })
            .collect::<Vec<_>>();
        self.unvalidated_paths.retain(|path| {
            !covered_candidates.iter().any(|covered| {
                path.environment_id == covered.environment_id
                    && (path.path == covered.path || path.path.starts_with(&covered.path))
            })
        });
        if clear_unknown_mutation {
            self.unvalidated_unknown_mutation = false;
        }

        self.has_successful_validation = true;
        let all_current_mutations_covered = !self.has_unvalidated_mutation();
        if all_current_mutations_covered {
            self.last_post_mutation_validation_status =
                ValidationFreshnessStatus::PassedAfterLastMutation;
        }
        all_current_mutations_covered
    }

    pub(crate) fn has_unvalidated_mutation(&self) -> bool {
        self.unvalidated_unknown_mutation || !self.unvalidated_paths.is_empty()
    }

    pub(crate) fn validation_freshness_status(&self) -> ValidationFreshnessStatus {
        if self.has_unvalidated_mutation() {
            self.last_post_mutation_validation_status.clone()
        } else if self.has_successful_validation {
            ValidationFreshnessStatus::PassedAfterLastMutation
        } else {
            ValidationFreshnessStatus::None
        }
    }

    fn clear_covered_paths(&mut self, covered_paths: &[PathBuf]) {
        self.unvalidated_paths.retain(|tracked| {
            !covered_paths
                .iter()
                .any(|covered| tracked.path == *covered || tracked.path.starts_with(covered))
        });
    }

    pub fn get_unified_diff(&self) -> Option<String> {
        self.unified_diff.clone()
    }

    pub(crate) fn has_unified_diff(&self) -> bool {
        self.unified_diff.is_some()
    }

    fn record_mutation(&mut self, paths: HashSet<TrackedPath>) {
        self.last_post_mutation_validation_status = if self.has_successful_validation {
            ValidationFreshnessStatus::StaleAfterLastMutation
        } else {
            ValidationFreshnessStatus::None
        };
        if paths.is_empty() {
            self.unvalidated_unknown_mutation = true;
        } else {
            self.unvalidated_paths.extend(paths);
        }
    }

    fn refresh_unified_diff(&mut self) {
        let rename_pairs = self.rename_pairs();
        let paired_destinations = rename_pairs.values().cloned().collect::<HashSet<_>>();
        let mut handled = HashSet::new();
        let mut paths = self
            .baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| self.display_path(path));
        paths.dedup();

        let mut previous_diffs = std::mem::take(&mut self.rendered_diffs);
        let mut rendered_diffs = HashMap::new();
        let mut aggregated = String::new();
        for path in paths {
            if !handled.insert(path.clone()) {
                continue;
            }

            if paired_destinations.contains(&path) {
                continue;
            }

            let (left_path, right_path) = if let Some(dest) = rename_pairs.get(&path) {
                handled.insert(dest.clone());
                (&path, dest)
            } else {
                (&path, &path)
            };

            let left_content = self.baseline_by_path.get(left_path);
            let right_content = self.current_by_path.get(right_path);
            let key = DiffCacheKey {
                left_path: left_path.clone(),
                left_revision: left_content.map(|content| content.revision),
                right_path: right_path.clone(),
                right_revision: right_content.map(|content| content.revision),
            };
            let rendered = previous_diffs.remove(&key).unwrap_or_else(|| {
                self.render_diff(
                    left_path,
                    left_content.map(|content| content.content.as_str()),
                    right_path,
                    right_content.map(|content| content.content.as_str()),
                )
            });

            if let Some(diff) = rendered.as_deref() {
                aggregated.push_str(diff);
                if !aggregated.ends_with('\n') {
                    aggregated.push('\n');
                }
            }
            rendered_diffs.insert(key, rendered);
        }

        self.rendered_diffs = rendered_diffs;
        self.unified_diff = (!aggregated.is_empty()).then_some(aggregated);
    }

    fn apply_change(&mut self, environment_id: &str, change: &AppliedPatchChange) {
        let source_path = TrackedPath::new(environment_id, change.path.as_path());
        match &change.change {
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => self.apply_add(source_path, content, overwritten_content.as_deref()),
            AppliedPatchFileChange::Delete { content } => self.apply_delete(source_path, content),
            AppliedPatchFileChange::Update {
                move_path,
                old_content,
                overwritten_move_content,
                new_content,
            } => {
                let move_path = move_path
                    .as_deref()
                    .map(|path| TrackedPath::new(environment_id, path));
                self.apply_update(
                    source_path,
                    move_path,
                    old_content,
                    overwritten_move_content.as_deref(),
                    new_content,
                )
            }
        }
    }

    fn apply_add(&mut self, path: TrackedPath, content: &str, overwritten_content: Option<&str>) {
        self.origin_by_current_path.remove(&path);
        if !self.current_by_path.contains_key(&path)
            && !self.baseline_by_path.contains_key(&path)
            && let Some(overwritten_content) = overwritten_content
        {
            let overwritten_content = self.tracked_content(overwritten_content);
            self.baseline_by_path
                .insert(path.clone(), overwritten_content);
        }
        let content = self.tracked_content(content);
        self.current_by_path.insert(path, content);
    }

    fn apply_delete(&mut self, path: TrackedPath, content: &str) {
        if self.current_by_path.remove(&path).is_none()
            && !self.baseline_by_path.contains_key(&path)
        {
            let content = self.tracked_content(content);
            self.baseline_by_path.insert(path.clone(), content);
        }
        self.origin_by_current_path.remove(&path);
    }

    fn apply_update(
        &mut self,
        source_path: TrackedPath,
        move_path: Option<TrackedPath>,
        old_content: &str,
        overwritten_move_content: Option<&str>,
        new_content: &str,
    ) {
        if !self.current_by_path.contains_key(&source_path)
            && !self.baseline_by_path.contains_key(&source_path)
        {
            let old_content = self.tracked_content(old_content);
            self.baseline_by_path
                .insert(source_path.clone(), old_content);
        }

        match move_path {
            Some(dest_path) => {
                if !self.current_by_path.contains_key(&dest_path)
                    && !self.baseline_by_path.contains_key(&dest_path)
                    && let Some(overwritten_move_content) = overwritten_move_content
                {
                    let overwritten_move_content = self.tracked_content(overwritten_move_content);
                    self.baseline_by_path
                        .insert(dest_path.clone(), overwritten_move_content);
                }
                let origin = self
                    .origin_by_current_path
                    .remove(&source_path)
                    .unwrap_or_else(|| source_path.clone());
                self.current_by_path.remove(&source_path);
                let new_content = self.tracked_content(new_content);
                self.current_by_path.insert(dest_path.clone(), new_content);
                self.origin_by_current_path.remove(&dest_path);
                if dest_path != origin {
                    self.origin_by_current_path.insert(dest_path, origin);
                }
            }
            None => {
                let new_content = self.tracked_content(new_content);
                self.current_by_path.insert(source_path, new_content);
            }
        }
    }

    fn tracked_content(&mut self, content: &str) -> TrackedContent {
        let revision = self.next_revision;
        self.next_revision += 1;
        TrackedContent {
            content: content.to_string(),
            revision,
        }
    }

    fn rename_pairs(&self) -> HashMap<TrackedPath, TrackedPath> {
        self.origin_by_current_path
            .iter()
            .filter_map(|(dest_path, origin_path)| {
                if dest_path == origin_path
                    || self.current_by_path.contains_key(origin_path)
                    || !self.current_by_path.contains_key(dest_path)
                    || !self.baseline_by_path.contains_key(origin_path)
                    || self.baseline_by_path.contains_key(dest_path)
                {
                    return None;
                }

                Some((origin_path.clone(), dest_path.clone()))
            })
            .collect()
    }

    fn render_diff(
        &self,
        left_path: &TrackedPath,
        left_content: Option<&str>,
        right_path: &TrackedPath,
        right_content: Option<&str>,
    ) -> Option<String> {
        if left_content == right_content {
            return None;
        }

        #[cfg(test)]
        self.rendered_diff_count
            .set(self.rendered_diff_count.get() + 1);

        let left_display = self.display_path(left_path);
        let right_display = self.display_path(right_path);
        let left_oid = left_content.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );
        let right_oid = right_content.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );

        let mut diff = format!("diff --git a/{left_display} b/{right_display}\n");
        match (left_content, right_content) {
            (None, Some(_)) => diff.push_str(&format!("new file mode {REGULAR_FILE_MODE}\n")),
            (Some(_), None) => diff.push_str(&format!("deleted file mode {REGULAR_FILE_MODE}\n")),
            (Some(_), Some(_)) => {}
            (None, None) => return None,
        }

        diff.push_str(&format!("index {left_oid}..{right_oid}\n"));

        let old_header = if left_content.is_some() {
            format!("a/{left_display}")
        } else {
            DEV_NULL.to_string()
        };
        let new_header = if right_content.is_some() {
            format!("b/{right_display}")
        } else {
            DEV_NULL.to_string()
        };

        let mut config = similar::TextDiff::configure();
        config.timeout(DIFF_TIMEOUT);
        let unified = config
            .diff_lines(left_content.unwrap_or(""), right_content.unwrap_or(""))
            .unified_diff()
            .context_radius(3)
            .header(&old_header, &new_header)
            .to_string();
        diff.push_str(&unified);
        Some(diff)
    }

    #[cfg(test)]
    fn rendered_diff_count(&self) -> usize {
        self.rendered_diff_count.get()
    }

    fn display_path(&self, path: &TrackedPath) -> String {
        let display = self
            .display_roots_by_environment
            .get(&path.environment_id)
            .and_then(|root| path.path.strip_prefix(root).ok())
            .unwrap_or(path.path.as_path());
        let display = display.display().to_string().replace('\\', "/");
        if self.display_roots_by_environment.len() > 1 && !path.environment_id.is_empty() {
            format!("{}/{display}", path.environment_id)
        } else {
            display
        }
    }
}

fn paths_touched_by_delta(environment_id: &str, delta: &AppliedPatchDelta) -> HashSet<TrackedPath> {
    let mut paths = HashSet::new();
    for change in delta.changes() {
        paths.insert(TrackedPath::new(environment_id, change.path.as_path()));
        if let AppliedPatchFileChange::Update {
            move_path: Some(move_path),
            ..
        } = &change.change
        {
            paths.insert(TrackedPath::new(environment_id, move_path));
        }
    }
    paths
}

fn is_validation_command(command: &[String]) -> bool {
    let tokens = normalized_command_tokens(command);
    is_validation_tokens(&tokens)
        || known_powershell_validation_tokens(command)
            .as_deref()
            .is_some_and(is_validation_tokens)
}

fn is_validation_tokens(tokens: &[String]) -> bool {
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return false;
    };
    if matches!(first, "echo" | "printf" | "write-output" | "#" | "rem") {
        return false;
    }
    if matches!(
        first,
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" | "cmd" | "cmd.exe"
    ) && let Some(position) = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "-command" | "-c" | "/c"))
    {
        return is_validation_tokens(&tokens[position.saturating_add(1)..]);
    }

    match first {
        "just" => matches!(
            tokens.get(1).map(String::as_str),
            Some(
                "test"
                    | "test-fast"
                    | "test-lane"
                    | "test-lane-fast"
                    | "check"
                    | "check-lane"
                    | "verify-local"
                    | "fix"
            )
        ),
        "cargo" => matches!(
            tokens.get(1).map(String::as_str),
            Some("test" | "nextest" | "check" | "shear" | "audit" | "deny" | "clippy" | "build")
        ),
        "nextest" => tokens.get(1).is_some_and(|token| token == "run"),
        "pytest" | "vitest" | "jest" | "mypy" | "tsc" | "eslint" => true,
        "python" => {
            tokens.get(1).is_some_and(|token| token == "-m")
                && tokens.get(2).is_some_and(|token| token == "pytest")
        }
        "uv" => {
            tokens.get(1).is_some_and(|token| token == "run") && is_validation_tokens(&tokens[2..])
        }
        "ruff" => tokens.get(1).is_some_and(|token| token == "check"),
        "playwright" => tokens.get(1).is_some_and(|token| token == "test"),
        "go" | "mvn" | "gradle" => tokens.get(1).is_some_and(|token| token == "test"),
        "dotnet" => tokens
            .get(1)
            .is_some_and(|token| matches!(token.as_str(), "test" | "build")),
        "npm" | "pnpm" | "yarn" => tokens.iter().skip(1).take(2).any(|token| {
            token.contains("test")
                || token.contains("lint")
                || token.contains("typecheck")
                || token.contains("build")
        }),
        _ => false,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ValidationCoverage {
    All,
    Paths(Vec<PathBuf>),
    ScopedUnknown,
}

fn validation_coverage(command: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    let tokens = normalized_command_tokens(command);
    let tokens = unwrap_command_tokens(&tokens);
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return ValidationCoverage::ScopedUnknown;
    };

    match first {
        "uv" if tokens.get(1).is_some_and(|token| token == "run") => {
            validation_coverage_for_tokens(&tokens[2..], cwd)
        }
        _ => validation_coverage_for_tokens(tokens, cwd),
    }
}

fn validation_coverage_for_tokens(tokens: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return ValidationCoverage::ScopedUnknown;
    };
    match first {
        "cargo" => cargo_validation_coverage(tokens, cwd),
        "just" => just_validation_coverage(tokens, cwd),
        "pytest" => path_validation_coverage(tokens, cwd, &["-k", "-m", "--lf"]),
        "python"
            if tokens.get(1).is_some_and(|token| token == "-m")
                && tokens.get(2).is_some_and(|token| token == "pytest") =>
        {
            path_validation_coverage(&tokens[2..], cwd, &["-k", "-m", "--lf"])
        }
        "eslint" | "mypy" | "playwright" | "vitest" | "jest" | "ruff" => {
            path_validation_coverage(tokens, cwd, &[])
        }
        "tsc" => project_flag_coverage(tokens, cwd),
        _ => ValidationCoverage::ScopedUnknown,
    }
}

fn unwrap_command_tokens(tokens: &[String]) -> &[String] {
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return tokens;
    };
    if matches!(
        first,
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" | "cmd" | "cmd.exe"
    ) && let Some(position) = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "-command" | "-c" | "/c"))
    {
        return &tokens[position.saturating_add(1)..];
    }
    if matches!(first, "bash" | "zsh" | "sh")
        && let Some(position) = tokens
            .iter()
            .position(|token| matches!(token.as_str(), "-c" | "-lc"))
    {
        return &tokens[position.saturating_add(1)..];
    }
    tokens
}

fn cargo_validation_coverage(tokens: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    if tokens
        .get(1)
        .is_some_and(|subcommand| matches!(subcommand.as_str(), "audit" | "deny" | "shear"))
    {
        return ValidationCoverage::ScopedUnknown;
    }
    if let Some(path) = flag_value(tokens, &["--manifest-path"]) {
        let manifest = resolve_scope_path(path, cwd);
        return ValidationCoverage::Paths(vec![
            manifest
                .parent()
                .unwrap_or(manifest.as_path())
                .to_path_buf(),
        ]);
    }
    if let Some(package) = flag_value(tokens, &["-p", "--package"]) {
        return package_validation_coverage(package, cwd);
    }
    if tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "--test" | "--tests" | "--bin" | "--bins" | "--example" | "--examples" | "--lib"
        )
    }) {
        return ValidationCoverage::ScopedUnknown;
    }

    let subcommand_index = if tokens.get(1).is_some_and(|token| token == "nextest") {
        2
    } else {
        1
    };
    if tokens
        .iter()
        .skip(subcommand_index + 1)
        .any(|token| !token.starts_with('-') && token != "--")
    {
        ValidationCoverage::ScopedUnknown
    } else {
        ValidationCoverage::All
    }
}

fn just_validation_coverage(tokens: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    let Some(recipe) = tokens.get(1).map(String::as_str) else {
        return ValidationCoverage::ScopedUnknown;
    };
    if recipe == "verify-local" {
        if tokens.iter().any(|token| token == "--plan") {
            return ValidationCoverage::ScopedUnknown;
        }
        let paths = tokens
            .iter()
            .filter_map(|token| token.strip_prefix("--changed="))
            .map(|path| resolve_scope_path(path, cwd))
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            return ValidationCoverage::Paths(paths);
        }
        if tokens.iter().any(|token| token == "--all-dirty") {
            return ValidationCoverage::All;
        }
        return ValidationCoverage::ScopedUnknown;
    }
    if let Some(package) = flag_value(tokens, &["-p", "--package"]) {
        return package_validation_coverage(package, cwd);
    }
    if recipe.contains("lane") || recipe.contains("package") {
        if let Some(package) = tokens.iter().skip(2).find(|token| !token.starts_with('-')) {
            return package_validation_coverage(package, cwd);
        }
        return ValidationCoverage::ScopedUnknown;
    }
    if tokens
        .iter()
        .any(|token| matches!(token.as_str(), "-e" | "--filter-expr"))
    {
        return ValidationCoverage::ScopedUnknown;
    }
    ValidationCoverage::All
}

fn package_validation_coverage(package: &str, cwd: Option<&Path>) -> ValidationCoverage {
    let Some(cwd) = cwd else {
        return ValidationCoverage::ScopedUnknown;
    };
    let directory = package.strip_prefix("codex-").unwrap_or(package);
    ValidationCoverage::Paths(vec![
        cwd.join(directory),
        cwd.join("codex-rs").join(directory),
    ])
}

fn path_validation_coverage(
    tokens: &[String],
    cwd: Option<&Path>,
    scoped_flags: &[&str],
) -> ValidationCoverage {
    if tokens
        .iter()
        .any(|token| scoped_flags.iter().any(|flag| token == flag))
    {
        return ValidationCoverage::ScopedUnknown;
    }
    let positional = tokens
        .iter()
        .skip(1)
        .filter(|token| !token.starts_with('-'))
        .collect::<Vec<_>>();
    let paths = positional
        .iter()
        .filter(|token| looks_like_scope_path(token))
        .map(|path| resolve_scope_path(path, cwd))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        if positional.is_empty() {
            ValidationCoverage::All
        } else {
            ValidationCoverage::ScopedUnknown
        }
    } else {
        ValidationCoverage::Paths(paths)
    }
}

fn project_flag_coverage(tokens: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    flag_value(tokens, &["-p", "--project"]).map_or(ValidationCoverage::All, |path| {
        ValidationCoverage::Paths(vec![resolve_scope_path(path, cwd)])
    })
}

fn flag_value<'a>(tokens: &'a [String], flags: &[&str]) -> Option<&'a str> {
    tokens.windows(2).find_map(|window| match window {
        [flag, value] if flags.iter().any(|candidate| flag == candidate) => Some(value.as_str()),
        _ => None,
    })
}

fn resolve_scope_path(path: &str, cwd: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.map_or(path.clone(), |cwd| cwd.join(path))
    }
}

fn looks_like_scope_path(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || Path::new(token).extension().is_some()
        || matches!(token, "." | "..")
}

fn is_broad_validation_filter_command(command: &[String]) -> bool {
    if !is_validation_command(command) {
        return false;
    }
    let expression = command
        .windows(2)
        .find_map(|window| match window {
            [flag, expression]
                if flag.eq_ignore_ascii_case("-e")
                    || flag.eq_ignore_ascii_case("--filter-expr") =>
            {
                Some(expression.clone())
            }
            _ => None,
        })
        .or_else(|| {
            let tokens = shell_filter_tokens(&command.join(" "));
            tokens.windows(2).find_map(|window| match window {
                [flag, expression]
                    if flag.eq_ignore_ascii_case("-e")
                        || flag.eq_ignore_ascii_case("--filter-expr") =>
                {
                    Some(expression.clone())
                }
                _ => None,
            })
        });
    let Some(expression) = expression else {
        return false;
    };
    let expression = expression.to_ascii_lowercase();
    expression.contains('|')
        || expression.contains(" or ")
        || expression.contains("package(")
        || expression.contains("kind(")
        || expression.contains("all()")
}

fn is_format_only_command(command: &[String]) -> bool {
    let tokens = normalized_command_tokens(command);
    matches!(
        tokens.as_slice(),
        [first, second, ..] if (first == "just" || first == "cargo") && second == "fmt"
    ) || matches!(
        tokens.first().map(String::as_str),
        Some("rustfmt" | "prettier")
    )
}

fn looks_like_mutating_command(command: &[String]) -> bool {
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    let mutating_format =
        is_format_only_command(command) && !unwrapped.iter().any(|token| token == "--check");
    let mutating_just_recipe = matches!(
        unwrapped,
        [first, second, ..]
            if first == "just"
                && matches!(second.as_str(), "fix" | "fix-lane" | "fix-workspace" | "fmt")
    );
    if mutating_format || mutating_just_recipe {
        return true;
    }
    if codex_shell_command::is_safe_command::is_known_safe_command(command) {
        return false;
    }

    let joined = command.join(" ").to_ascii_lowercase();
    if joined.contains(">>") || joined.contains(" > ") || joined.contains("| out-file") {
        return true;
    }

    let tokens = shell_filter_tokens(&joined);
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "apply_patch"
                | "add-content"
                | "copy-item"
                | "cp"
                | "del"
                | "erase"
                | "md"
                | "mkdir"
                | "move-item"
                | "mv"
                | "new-item"
                | "ni"
                | "out-file"
                | "rd"
                | "reg"
                | "remove-item"
                | "ren"
                | "rename-item"
                | "rm"
                | "rmdir"
                | "set-content"
                | "set-item"
                | "set-itemproperty"
                | "tee"
                | "tee-object"
        )
    }) {
        return true;
    }

    tokens.windows(2).any(|window| match window {
        [first, second]
            if command_basename(first) == "git"
                && matches!(
                    second.as_str(),
                    "add"
                        | "apply"
                        | "checkout"
                        | "clean"
                        | "commit"
                        | "merge"
                        | "mv"
                        | "rebase"
                        | "reset"
                        | "restore"
                        | "rm"
                        | "switch"
                ) =>
        {
            true
        }
        [first, second]
            if matches!(command_basename(first), "npm" | "pnpm" | "yarn")
                && matches!(
                    second.as_str(),
                    "add" | "install" | "remove" | "uninstall" | "update"
                ) =>
        {
            true
        }
        _ => false,
    })
}

fn normalized_command_tokens(command: &[String]) -> Vec<String> {
    command
        .iter()
        .flat_map(|token| token.split_whitespace())
        .map(|part| {
            part.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | '&' | '|'
                )
            })
            .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

fn command_basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

fn known_powershell_validation_tokens(command: &[String]) -> Option<Vec<String>> {
    let script = command.join("\n");
    let lower = script.to_ascii_lowercase();
    if !(lower.contains("$lastexitcode") && lower.contains("exit $code")) {
        return None;
    }
    script.lines().find_map(|line| {
        let lower = line.trim().to_ascii_lowercase();
        if !(lower.starts_with("$out = &") || lower.starts_with("$output = &")) {
            return None;
        }
        let (_, command) = line.split_once('&')?;
        Some(
            shell_filter_tokens(command)
                .into_iter()
                .map(|token| token.to_ascii_lowercase())
                .filter(|token| token != "2>&1" && token != "2>&")
                .collect(),
        )
    })
}

fn shell_filter_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in command.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn git_blob_oid(data: &[u8]) -> String {
    format!("{:x}", git_blob_sha1_hex_bytes(data))
}

/// Compute the Git SHA-1 blob object ID for the given content (bytes).
fn git_blob_sha1_hex_bytes(data: &[u8]) -> Output<sha1::Sha1> {
    let header = format!("blob {}\0", data.len());
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
#[path = "turn_diff_tracker_tests.rs"]
mod tests;
