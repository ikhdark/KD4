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
struct ValidationRecord {
    kind: ValidationCommandKind,
    command: Vec<String>,
    covered_paths: HashSet<TrackedPath>,
    covered_unknown_mutation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ValidationFreshnessStatus {
    None,
    StaleAfterLastMutation,
    FormatOnly,
    AdvisoryBroadFilter,
    PassedAfterLastMutation,
    FailedAfterLastMutation,
    TimedOut,
}

impl ValidationFreshnessStatus {
    pub(crate) fn final_warning_message(&self) -> Option<&'static str> {
        match self {
            Self::PassedAfterLastMutation => None,
            Self::None => Some(
                "Changed files have not been followed by post-change validation evidence. Final answers should state what commands passed, failed, or were skipped; formatting-only commands do not count as correctness validation.",
            ),
            Self::StaleAfterLastMutation => Some(
                "Changed files were modified after the last successful validation evidence, so that evidence is stale. State what passed before the later edit and what remains unvalidated.",
            ),
            Self::FormatOnly => Some(
                "Changed files have only been followed by formatting commands. Formatting-only commands do not prove correctness or runtime reachability.",
            ),
            Self::AdvisoryBroadFilter => Some(
                "Changed files have only been followed by broad validation evidence. Broad test filters are advisory and do not prove focused correctness by themselves.",
            ),
            Self::FailedAfterLastMutation => Some(
                "Changed files were followed by validation that failed. State what passed, what failed, and what remains unvalidated.",
            ),
            Self::TimedOut => Some(
                "Changed files were followed by validation that timed out. State what passed, what timed out, and what remains unvalidated.",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationCommandKind {
    Test,
    Check,
    Build,
    Lint,
    Typecheck,
    DependencyHygiene,
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
    unified_diff_fallback_needed: bool,
    unvalidated_paths: HashSet<TrackedPath>,
    unvalidated_unknown_mutation: bool,
    validation_records: Vec<ValidationRecord>,
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
            unified_diff_fallback_needed: false,
            unvalidated_paths: HashSet::new(),
            unvalidated_unknown_mutation: false,
            validation_records: Vec::new(),
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
        if self.has_unvalidated_mutation() {
            self.unified_diff_fallback_needed = true;
        }
        self.valid = false;
        self.rendered_diffs.clear();
        self.unified_diff = None;
    }

    pub(crate) fn record_unknown_mutation(&mut self) {
        self.record_unknown_mutation_index();
        self.invalidate();
    }

    pub(crate) fn record_exec_command_end(
        &mut self,
        command: &[String],
        exit_code: i32,
        timed_out: bool,
    ) {
        let is_post_mutation = self.has_unvalidated_mutation();
        let validation_kind = classify_validation_command(command);
        let format_only = is_format_only_command(command);
        let broad_filter = is_broad_validation_filter_command(command);

        if is_post_mutation {
            self.last_post_mutation_validation_status = if format_only {
                ValidationFreshnessStatus::FormatOnly
            } else if timed_out && validation_kind.is_some() {
                ValidationFreshnessStatus::TimedOut
            } else if validation_kind.is_some() && exit_code == 0 && broad_filter {
                ValidationFreshnessStatus::AdvisoryBroadFilter
            } else if validation_kind.is_some() && exit_code == 0 {
                ValidationFreshnessStatus::PassedAfterLastMutation
            } else if validation_kind.is_some() {
                ValidationFreshnessStatus::FailedAfterLastMutation
            } else {
                self.last_post_mutation_validation_status.clone()
            };
        }

        if exit_code == 0
            && !timed_out
            && !broad_filter
            && let Some(kind) = validation_kind
        {
            self.validation_records.push(ValidationRecord {
                kind,
                command: command.to_vec(),
                covered_paths: self.unvalidated_paths.clone(),
                covered_unknown_mutation: self.unvalidated_unknown_mutation,
            });
            self.unvalidated_paths.clear();
            self.unvalidated_unknown_mutation = false;
        }
    }

    pub(crate) fn record_verified_validation(
        &mut self,
        command: Vec<String>,
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
                    let absolute_path = root.join(path);
                    candidates.push(TrackedPath::new(environment_id, absolute_path.as_path()));
                }
                candidates
            })
            .collect::<HashSet<_>>();
        let covered_paths = self
            .unvalidated_paths
            .intersection(&covered_candidates)
            .cloned()
            .collect::<HashSet<_>>();
        let covered_unknown_mutation = clear_unknown_mutation && self.unvalidated_unknown_mutation;

        self.validation_records.push(ValidationRecord {
            kind: ValidationCommandKind::Check,
            command,
            covered_paths: covered_paths.clone(),
            covered_unknown_mutation,
        });
        for path in covered_paths {
            self.unvalidated_paths.remove(&path);
        }
        if covered_unknown_mutation {
            self.unvalidated_unknown_mutation = false;
        }
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

    pub(crate) fn needs_unified_diff_fallback(&self) -> bool {
        self.unified_diff.is_none() && self.unified_diff_fallback_needed
    }

    pub(crate) fn validation_freshness_status(&self) -> ValidationFreshnessStatus {
        if self.has_unvalidated_mutation() {
            self.last_post_mutation_validation_status.clone()
        } else if self.validation_records.is_empty() {
            ValidationFreshnessStatus::None
        } else {
            ValidationFreshnessStatus::PassedAfterLastMutation
        }
    }

    pub(crate) fn final_validation_warning_message(&self) -> Option<&'static str> {
        if !self.has_unvalidated_mutation() && self.validation_records.is_empty() {
            return None;
        }
        self.validation_freshness_status().final_warning_message()
    }

    pub(crate) fn command_looks_like_mutating(command: &[String]) -> bool {
        if classify_validation_command(command).is_some() || looks_like_context_evidence(command) {
            return false;
        }
        if is_format_only_command(command) {
            return true;
        }

        let lower = command.join(" ").to_ascii_lowercase();
        if lower.contains(">>") || lower.contains(" > ") || lower.contains("| out-file") {
            return true;
        }

        let tokens = normalized_command_tokens(command);
        if tokens.iter().any(|token| {
            matches!(
                token.as_str(),
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

        tokens.windows(2).any(|window| {
            matches!(
                window,
                [first, second]
                    if first == "git"
                        && matches!(
                            second.as_str(),
                            "add" | "apply" | "checkout" | "clean" | "commit" | "merge" | "mv" | "rebase" | "reset" | "restore" | "rm" | "switch"
                        )
            ) || matches!(
                window,
                [first, second]
                    if matches!(first.as_str(), "npm" | "pnpm" | "yarn")
                        && matches!(second.as_str(), "add" | "install" | "remove" | "uninstall" | "update")
            )
        })
    }

    pub fn get_unified_diff(&self) -> Option<String> {
        self.unified_diff.clone()
    }

    pub(crate) fn has_unified_diff(&self) -> bool {
        self.unified_diff.is_some()
    }

    fn record_mutation(&mut self, paths: HashSet<TrackedPath>) {
        self.last_post_mutation_validation_status = if self.validation_records.is_empty() {
            ValidationFreshnessStatus::None
        } else {
            ValidationFreshnessStatus::StaleAfterLastMutation
        };
        if !self.valid {
            self.unified_diff_fallback_needed = true;
        }
        if paths.is_empty() {
            self.unvalidated_unknown_mutation = true;
        } else {
            self.unvalidated_paths.extend(paths);
        }
    }

    fn record_unknown_mutation_index(&mut self) {
        self.last_post_mutation_validation_status = if self.validation_records.is_empty() {
            ValidationFreshnessStatus::None
        } else {
            ValidationFreshnessStatus::StaleAfterLastMutation
        };
        self.unvalidated_unknown_mutation = true;
        if !self.valid {
            self.unified_diff_fallback_needed = true;
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
        if self.unified_diff.is_some() {
            self.unified_diff_fallback_needed = false;
        }
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

fn classify_validation_command(command: &[String]) -> Option<ValidationCommandKind> {
    let tokens = normalized_command_tokens(command);
    classify_validation_tokens(&tokens).or_else(|| {
        known_powershell_logical_validation_tokens(command)
            .as_deref()
            .and_then(classify_validation_tokens)
    })
}

fn is_broad_validation_filter_command(command: &[String]) -> bool {
    let tokens = normalized_command_tokens(command);
    if classify_validation_tokens(&tokens).is_none() {
        return false;
    }
    let Some(expression) = filter_expression_after_flag(command, "-e")
        .or_else(|| filter_expression_after_flag(command, "--filter-expr"))
    else {
        return false;
    };
    let expression = expression.to_ascii_lowercase();
    expression.contains('|')
        || expression.contains(" or ")
        || expression.contains("package(")
        || expression.contains("kind(")
        || expression.contains("all()")
}

fn filter_expression_after_flag(command: &[String], flag: &str) -> Option<String> {
    command
        .windows(2)
        .find_map(|window| match window {
            [candidate, expression] if candidate.eq_ignore_ascii_case(flag) => {
                Some(expression.clone())
            }
            _ => None,
        })
        .or_else(|| filter_expression_after_flag_in_joined_command(command, flag))
}

fn filter_expression_after_flag_in_joined_command(
    command: &[String],
    flag: &str,
) -> Option<String> {
    let joined = command.join(" ");
    let tokens = shell_filter_tokens(&joined);
    tokens.windows(2).find_map(|window| match window {
        [candidate, expression] if candidate.eq_ignore_ascii_case(flag) => Some(expression.clone()),
        _ => None,
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
                push_filter_token(&mut tokens, &mut current);
            }
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => {
                push_filter_token(&mut tokens, &mut current);
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                push_filter_token(&mut tokens, &mut current);
            }
            None => current.push(ch),
        }
    }
    push_filter_token(&mut tokens, &mut current);
    tokens
}

fn push_filter_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn is_format_only_command(command: &[String]) -> bool {
    let tokens = normalized_command_tokens(command);
    match tokens.as_slice() {
        [first, second, ..] if first == "just" && second == "fmt" => true,
        [first, second, ..] if first == "cargo" && second == "fmt" => true,
        [first, ..] if matches!(first.as_str(), "rustfmt" | "prettier") => true,
        _ => false,
    }
}

fn looks_like_context_evidence(command: &[String]) -> bool {
    let lower = command.join(" ").to_ascii_lowercase();
    lower.contains("rg ")
        || lower.starts_with("rg ")
        || lower.contains("select-string")
        || lower.contains("findstr")
        || lower.contains("git status")
        || lower.contains("git diff")
        || lower.contains("git show")
        || lower.contains("get-content")
        || lower.contains("readlines")
        || lower.contains("read_file_span")
}

fn normalized_command_tokens(command: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for token in command {
        for part in token.split_whitespace() {
            let cleaned = part
                .trim_matches(|ch: char| {
                    matches!(
                        ch,
                        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | '&' | '|'
                    )
                })
                .to_ascii_lowercase();
            if !cleaned.is_empty() {
                tokens.push(cleaned);
            }
        }
    }
    tokens
}

fn known_powershell_logical_validation_tokens(command: &[String]) -> Option<Vec<String>> {
    let script = command.join("\n");
    let lower = script.to_ascii_lowercase();
    if !(lower.contains("$lastexitcode") && lower.contains("exit $code")) {
        return None;
    }

    script.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("$out = &")
            .or_else(|| trimmed.strip_prefix("$output = &"))?
            .trim();
        let mut tokens = shell_filter_tokens(rest);
        while tokens
            .last()
            .is_some_and(|token| token == "2>&1" || token == "2>&")
        {
            tokens.pop();
        }
        let tokens = tokens
            .into_iter()
            .map(|token| {
                token
                    .trim_matches(|ch: char| {
                        matches!(
                            ch,
                            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | '&' | '|'
                        )
                    })
                    .to_ascii_lowercase()
            })
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        matches!(tokens.first().map(String::as_str), Some("just")).then_some(tokens)
    })
}

fn classify_validation_tokens(tokens: &[String]) -> Option<ValidationCommandKind> {
    let first = tokens.first()?.as_str();
    if matches!(first, "echo" | "printf" | "write-output" | "#" | "rem") {
        return None;
    }
    if matches!(
        first,
        "pwsh" | "powershell" | "powershell.exe" | "cmd" | "cmd.exe"
    ) && let Some(position) = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "-command" | "-c" | "/c"))
    {
        return classify_validation_tokens(&tokens[position.saturating_add(1)..]);
    }

    match first {
        "just" => classify_just_validation(tokens),
        "cargo" => classify_cargo_validation(tokens),
        "nextest" if token_at(tokens, 1) == Some("run") => Some(ValidationCommandKind::Test),
        "npm" | "pnpm" | "yarn" => classify_node_validation(tokens),
        "pytest" | "vitest" | "jest" => Some(ValidationCommandKind::Test),
        "python" if token_at(tokens, 1) == Some("-m") && token_at(tokens, 2) == Some("pytest") => {
            Some(ValidationCommandKind::Test)
        }
        "python"
            if token_at(tokens, 1) == Some("-m") && token_at(tokens, 2) == Some("unittest") =>
        {
            Some(ValidationCommandKind::Test)
        }
        "uv" if token_at(tokens, 1) == Some("run") => classify_validation_tokens(&tokens[2..]),
        "ruff" if token_at(tokens, 1) == Some("check") => Some(ValidationCommandKind::Lint),
        "mypy" | "tsc" => Some(ValidationCommandKind::Typecheck),
        "eslint" => Some(ValidationCommandKind::Lint),
        "playwright" if token_at(tokens, 1) == Some("test") => Some(ValidationCommandKind::Test),
        "go" if token_at(tokens, 1) == Some("test") => Some(ValidationCommandKind::Test),
        "dotnet" if token_at(tokens, 1) == Some("test") => Some(ValidationCommandKind::Test),
        "dotnet" if token_at(tokens, 1) == Some("build") => Some(ValidationCommandKind::Build),
        "mvn" if token_at(tokens, 1) == Some("test") => Some(ValidationCommandKind::Test),
        "gradle" if token_at(tokens, 1) == Some("test") => Some(ValidationCommandKind::Test),
        _ => None,
    }
}

fn classify_just_validation(tokens: &[String]) -> Option<ValidationCommandKind> {
    match token_at(tokens, 1)? {
        "test" | "test-fast" | "test-lane" | "test-lane-fast" => Some(ValidationCommandKind::Test),
        "check" | "check-lane" => Some(ValidationCommandKind::Check),
        "fix" => Some(ValidationCommandKind::Lint),
        _ => None,
    }
}

fn classify_cargo_validation(tokens: &[String]) -> Option<ValidationCommandKind> {
    match token_at(tokens, 1)? {
        "test" | "nextest" => Some(ValidationCommandKind::Test),
        "check" => Some(ValidationCommandKind::Check),
        "shear" | "audit" | "deny" => Some(ValidationCommandKind::DependencyHygiene),
        "clippy" => Some(ValidationCommandKind::Lint),
        "build" => Some(ValidationCommandKind::Build),
        _ => None,
    }
}

fn classify_node_validation(tokens: &[String]) -> Option<ValidationCommandKind> {
    let script = if token_at(tokens, 1) == Some("run") {
        token_at(tokens, 2)?
    } else {
        token_at(tokens, 1)?
    };
    if script.contains("test") {
        Some(ValidationCommandKind::Test)
    } else if script.contains("lint") {
        Some(ValidationCommandKind::Lint)
    } else if script.contains("typecheck") {
        Some(ValidationCommandKind::Typecheck)
    } else if script.contains("build") {
        Some(ValidationCommandKind::Build)
    } else {
        None
    }
}

fn token_at(tokens: &[String], index: usize) -> Option<&str> {
    tokens.get(index).map(String::as_str)
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
