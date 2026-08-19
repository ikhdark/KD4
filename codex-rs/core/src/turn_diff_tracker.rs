use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::Duration;

use codex_utils_path::normalize_for_path_comparison;
use sha1::digest::Output;
#[cfg(test)]
use sha2::Digest;
#[cfg(test)]
use sha2::Sha256;

use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DEV_NULL: &str = "/dev/null";
const REGULAR_FILE_MODE: &str = "100644";
// Normal edits finish well within 100 ms; pathological inputs fall back to a coarse,
// content-exact diff without stalling tool completion.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(test)]
const POST_EDIT_BUNDLE_MAX_DIFF_BYTES: usize = 8 * 1024;

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CoherentImplementationBoundary {
    pub(crate) candidate_identity: String,
    pub(crate) batch_closed: bool,
    pub(crate) implementation_obligations_satisfied: bool,
    pub(crate) pending_mutation_obligations: bool,
    pub(crate) typed_children_quiescent: bool,
    pub(crate) default_children_quiescent: bool,
}

#[cfg(test)]
impl CoherentImplementationBoundary {
    fn trustworthy_quiescence(&self) -> bool {
        self.batch_closed
            && self.implementation_obligations_satisfied
            && !self.pending_mutation_obligations
            && self.typed_children_quiescent
            && self.default_children_quiescent
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PostEditInspectionDependencies {
    pub(crate) requirement_manifest_identity: String,
    pub(crate) proof_route_identity: String,
    pub(crate) validation_identity: String,
    pub(crate) rendered_gate_identity: String,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PostEditInspectionOutcome {
    AcceptForFocusedValidation,
    RequestRepairBatch { repair_identity: String },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PostEditBundleSection {
    Diff,
    Requirements,
    ProofAndValidation,
    CompletionGates,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PostEditInspectionBundle {
    pub(crate) fingerprint: String,
    pub(crate) candidate_identity: String,
    pub(crate) final_mutation_revision: u64,
    pub(crate) diff_identity: String,
    pub(crate) bounded_diff: String,
    pub(crate) dependencies: PostEditInspectionDependencies,
    pub(crate) rebuilt_sections: Vec<PostEditBundleSection>,
    pub(crate) outcome: Option<PostEditInspectionOutcome>,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PostEditInspectionPreparation {
    FailOpen,
    Ready(Box<PostEditInspectionBundle>),
    Suppressed(PostEditInspectionOutcome),
}

struct TrackedContent {
    content: String,
    mode: Option<String>,
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
            path: normalize_tracked_path(path),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandMutation {
    ReadOnly,
    KnownMutation { paths: Option<BTreeSet<PathBuf>> },
    Uncertain,
}

impl CommandMutation {
    pub(crate) fn may_have_mutated(&self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    pub(crate) fn paths(&self) -> Option<&BTreeSet<PathBuf>> {
        match self {
            Self::KnownMutation { paths } => paths.as_ref(),
            Self::ReadOnly | Self::Uncertain => None,
        }
    }
}

impl From<bool> for CommandMutation {
    fn from(possible_mutation: bool) -> Self {
        if possible_mutation {
            Self::KnownMutation { paths: None }
        } else {
            Self::ReadOnly
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
    mutation_revision: u64,
    rendered_diffs: HashMap<DiffCacheKey, Option<String>>,
    unified_diff: Option<String>,
    unvalidated_paths: HashSet<TrackedPath>,
    unvalidated_unknown_mutation: bool,
    has_successful_validation: bool,
    last_successful_validation_revision: Option<u64>,
    last_post_mutation_validation_status: ValidationFreshnessStatus,
    #[cfg(test)]
    post_edit_inspection_bundle: Option<PostEditInspectionBundle>,
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
            mutation_revision: 0,
            rendered_diffs: HashMap::new(),
            unified_diff: None,
            unvalidated_paths: HashSet::new(),
            unvalidated_unknown_mutation: false,
            has_successful_validation: false,
            last_successful_validation_revision: None,
            last_post_mutation_validation_status: ValidationFreshnessStatus::None,
            #[cfg(test)]
            post_edit_inspection_bundle: None,
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
        tracker.display_roots_by_environment = display_roots
            .into_iter()
            .map(|(environment_id, root)| (environment_id, normalize_tracked_path(&root)))
            .collect();
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
        environment_id: &str,
        cwd: Option<&Path>,
    ) {
        let mutation = command_mutation(command, cwd);
        self.record_exec_command_end_with_mutation_at(
            command,
            exit_code,
            timed_out,
            environment_id,
            cwd,
            mutation,
        );
    }

    pub(crate) fn record_exec_command_end_with_mutation_at(
        &mut self,
        command: &[String],
        exit_code: i32,
        timed_out: bool,
        environment_id: &str,
        cwd: Option<&Path>,
        mutation: CommandMutation,
    ) {
        let was_post_mutation = self.has_unvalidated_mutation();
        let is_validation = is_validation_command(command);
        let format_only = is_format_only_command(command);
        let broad_filter = is_broad_validation_filter_command(command);
        let possible_mutation = mutation.may_have_mutated();

        // A command can write before failing or timing out, so known mutators
        // always invalidate exact diff/freshness state.
        match mutation {
            CommandMutation::KnownMutation { paths: Some(paths) } => {
                self.record_mutation(
                    paths
                        .iter()
                        .map(|path| TrackedPath::new(environment_id, path))
                        .collect(),
                );
                self.invalidate();
            }
            CommandMutation::KnownMutation { paths: None } | CommandMutation::Uncertain => {
                self.record_unknown_mutation();
            }
            CommandMutation::ReadOnly => {}
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
                    self.clear_environment_paths(environment_id);
                    if self.can_clear_unknown_for_environment(environment_id) {
                        self.unvalidated_unknown_mutation = false;
                    }
                }
                ValidationCoverage::Paths(paths) => {
                    self.clear_covered_paths(environment_id, &paths);
                }
                ValidationCoverage::ScopedUnknown => {}
            }
            self.last_post_mutation_validation_status = if self.has_unvalidated_mutation() {
                ValidationFreshnessStatus::ScopedValidationIncomplete
            } else {
                ValidationFreshnessStatus::PassedAfterLastMutation
            };
            if self.last_post_mutation_validation_status
                == ValidationFreshnessStatus::PassedAfterLastMutation
            {
                self.last_successful_validation_revision = Some(self.mutation_revision);
            }
        }
    }

    pub(crate) fn has_unvalidated_mutation(&self) -> bool {
        self.unvalidated_unknown_mutation || !self.unvalidated_paths.is_empty()
    }

    pub(crate) fn current_mutation_revision(&self) -> u64 {
        self.mutation_revision
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

    pub(crate) fn last_successful_validation_revision(&self) -> Option<u64> {
        self.last_successful_validation_revision
    }

    #[cfg(test)]
    pub(crate) fn prepare_post_edit_inspection(
        &mut self,
        boundary: Option<&CoherentImplementationBoundary>,
        dependencies: PostEditInspectionDependencies,
    ) -> PostEditInspectionPreparation {
        let Some(boundary) = boundary.filter(|boundary| boundary.trustworthy_quiescence()) else {
            return PostEditInspectionPreparation::FailOpen;
        };
        let diff = self.unified_diff.clone().unwrap_or_default();
        let diff_identity = format!("{:x}", Sha256::digest(diff.as_bytes()));
        let fingerprint = post_edit_fingerprint(
            boundary,
            self.mutation_revision,
            &diff_identity,
            &dependencies,
        );
        if let Some(existing) = self.post_edit_inspection_bundle.as_ref()
            && existing.fingerprint == fingerprint
        {
            return existing.outcome.clone().map_or_else(
                || PostEditInspectionPreparation::Ready(Box::new(existing.clone())),
                PostEditInspectionPreparation::Suppressed,
            );
        }
        let bounded_diff = if diff.len() <= POST_EDIT_BUNDLE_MAX_DIFF_BYTES {
            diff
        } else {
            let mut end = POST_EDIT_BUNDLE_MAX_DIFF_BYTES;
            while end > 0 && !diff.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n[diff truncated]", &diff[..end])
        };
        let rebuilt_sections = self.post_edit_inspection_bundle.as_ref().map_or_else(
            || {
                vec![
                    PostEditBundleSection::Diff,
                    PostEditBundleSection::Requirements,
                    PostEditBundleSection::ProofAndValidation,
                    PostEditBundleSection::CompletionGates,
                ]
            },
            |existing| {
                let mut sections = Vec::new();
                if existing.candidate_identity != boundary.candidate_identity
                    || existing.final_mutation_revision != self.mutation_revision
                    || existing.diff_identity != diff_identity
                {
                    sections.push(PostEditBundleSection::Diff);
                }
                if existing.dependencies.requirement_manifest_identity
                    != dependencies.requirement_manifest_identity
                {
                    sections.push(PostEditBundleSection::Requirements);
                }
                if existing.dependencies.proof_route_identity != dependencies.proof_route_identity
                    || existing.dependencies.validation_identity != dependencies.validation_identity
                {
                    sections.push(PostEditBundleSection::ProofAndValidation);
                }
                if existing.dependencies.rendered_gate_identity
                    != dependencies.rendered_gate_identity
                {
                    sections.push(PostEditBundleSection::CompletionGates);
                }
                sections
            },
        );
        let bundle = PostEditInspectionBundle {
            fingerprint,
            candidate_identity: boundary.candidate_identity.clone(),
            final_mutation_revision: self.mutation_revision,
            diff_identity,
            bounded_diff,
            dependencies,
            rebuilt_sections,
            outcome: None,
        };
        self.post_edit_inspection_bundle = Some(bundle.clone());
        PostEditInspectionPreparation::Ready(Box::new(bundle))
    }

    #[cfg(test)]
    pub(crate) fn record_post_edit_inspection_outcome(
        &mut self,
        fingerprint: &str,
        outcome: PostEditInspectionOutcome,
    ) -> bool {
        let Some(bundle) = self.post_edit_inspection_bundle.as_mut() else {
            return false;
        };
        if bundle.fingerprint != fingerprint {
            return false;
        }
        bundle.outcome = Some(outcome);
        true
    }

    fn clear_covered_paths(&mut self, environment_id: &str, covered_paths: &[PathBuf]) {
        let covered_paths = covered_paths
            .iter()
            .map(|path| normalize_tracked_path(path))
            .collect::<Vec<_>>();
        self.unvalidated_paths.retain(|tracked| {
            tracked.environment_id != environment_id
                || !covered_paths
                    .iter()
                    .any(|covered| tracked.path == *covered || tracked.path.starts_with(covered))
        });
    }

    fn clear_environment_paths(&mut self, environment_id: &str) {
        self.unvalidated_paths
            .retain(|tracked| tracked.environment_id != environment_id);
    }

    fn can_clear_unknown_for_environment(&self, environment_id: &str) -> bool {
        let mut known_environments = self
            .display_roots_by_environment
            .keys()
            .map(String::as_str)
            .chain(
                self.unvalidated_paths
                    .iter()
                    .map(|tracked| tracked.environment_id.as_str()),
            )
            .collect::<HashSet<_>>();
        known_environments.retain(|known| !known.is_empty());
        known_environments.is_empty()
            || (known_environments.len() == 1 && known_environments.contains(environment_id))
    }

    pub fn get_unified_diff(&self) -> Option<String> {
        self.unified_diff.clone()
    }

    pub(crate) fn has_unified_diff(&self) -> bool {
        self.unified_diff.is_some()
    }

    fn record_mutation(&mut self, paths: HashSet<TrackedPath>) {
        self.mutation_revision = self.mutation_revision.saturating_add(1);
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
                self.render_diff(left_path, left_content, right_path, right_content)
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
            let overwritten_content = self.tracked_content(&path, overwritten_content);
            self.baseline_by_path
                .insert(path.clone(), overwritten_content);
        }
        let content = self.tracked_content(&path, content);
        self.current_by_path.insert(path, content);
    }

    fn apply_delete(&mut self, path: TrackedPath, content: &str) {
        if self.current_by_path.remove(&path).is_none()
            && !self.baseline_by_path.contains_key(&path)
        {
            let content = self.tracked_content(&path, content);
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
            let old_content = self.tracked_content(&source_path, old_content);
            self.baseline_by_path
                .insert(source_path.clone(), old_content);
        }

        match move_path {
            Some(dest_path) => {
                if !self.current_by_path.contains_key(&dest_path)
                    && !self.baseline_by_path.contains_key(&dest_path)
                    && let Some(overwritten_move_content) = overwritten_move_content
                {
                    let overwritten_move_content =
                        self.tracked_content(&dest_path, overwritten_move_content);
                    self.baseline_by_path
                        .insert(dest_path.clone(), overwritten_move_content);
                }
                let origin = self
                    .origin_by_current_path
                    .remove(&source_path)
                    .unwrap_or_else(|| source_path.clone());
                self.current_by_path.remove(&source_path);
                let new_content = self.tracked_content(&dest_path, new_content);
                self.current_by_path.insert(dest_path.clone(), new_content);
                self.origin_by_current_path.remove(&dest_path);
                if dest_path != origin {
                    self.origin_by_current_path.insert(dest_path, origin);
                }
            }
            None => {
                let new_content = self.tracked_content(&source_path, new_content);
                self.current_by_path.insert(source_path, new_content);
            }
        }
    }

    fn tracked_content(&mut self, path: &TrackedPath, content: &str) -> TrackedContent {
        let mode = self
            .current_by_path
            .get(path)
            .and_then(|tracked| tracked.mode.clone())
            .or_else(|| {
                self.baseline_by_path
                    .get(path)
                    .and_then(|tracked| tracked.mode.clone())
            })
            .or_else(|| self.file_mode(path).map(str::to_owned));
        let revision = self.next_revision;
        self.next_revision += 1;
        TrackedContent {
            content: content.to_string(),
            mode,
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
        left_content: Option<&TrackedContent>,
        right_path: &TrackedPath,
        right_content: Option<&TrackedContent>,
    ) -> Option<String> {
        let left_text = left_content.map(|content| content.content.as_str());
        let right_text = right_content.map(|content| content.content.as_str());
        if left_text == right_text {
            return None;
        }

        #[cfg(test)]
        self.rendered_diff_count
            .set(self.rendered_diff_count.get() + 1);

        let left_display = self.display_path(left_path);
        let right_display = self.display_path(right_path);
        let left_oid = left_text.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );
        let right_oid = right_text.map_or_else(
            || ZERO_OID.to_string(),
            |content| git_blob_oid(content.as_bytes()),
        );
        let mut diff = format!("diff --git a/{left_display} b/{right_display}\n");
        match (left_content, right_content) {
            (None, Some(_)) => {
                let mode = right_content
                    .and_then(|content| content.mode.as_deref())
                    .or_else(|| self.file_mode(right_path))
                    .unwrap_or(REGULAR_FILE_MODE);
                diff.push_str(&format!("new file mode {mode}\n"));
            }
            (Some(_), None) => {
                let mode = left_content
                    .and_then(|content| content.mode.as_deref())
                    .or_else(|| self.file_mode(left_path))
                    .unwrap_or(REGULAR_FILE_MODE);
                diff.push_str(&format!("deleted file mode {mode}\n"));
            }
            (Some(_), Some(_)) => {}
            (None, None) => return None,
        }

        diff.push_str(&format!("index {left_oid}..{right_oid}\n"));

        let old_header = if left_text.is_some() {
            format!("a/{left_display}")
        } else {
            DEV_NULL.to_string()
        };
        let new_header = if right_text.is_some() {
            format!("b/{right_display}")
        } else {
            DEV_NULL.to_string()
        };

        let mut config = similar::TextDiff::configure();
        config.timeout(DIFF_TIMEOUT);
        let unified = config
            .diff_lines(left_text.unwrap_or(""), right_text.unwrap_or(""))
            .unified_diff()
            .context_radius(3)
            .header(&old_header, &new_header)
            .to_string();
        diff.push_str(&unified);
        Some(diff)
    }

    fn file_mode(&self, path: &TrackedPath) -> Option<&'static str> {
        let filesystem_path = if path.path.is_absolute() {
            path.path.clone()
        } else {
            self.display_roots_by_environment
                .get(&path.environment_id)
                .map_or_else(|| path.path.clone(), |root| root.join(&path.path))
        };
        if let Ok(metadata) = std::fs::symlink_metadata(&filesystem_path) {
            if metadata.file_type().is_symlink() {
                return Some("120000");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    return Some("100755");
                }
            }
            return Some(REGULAR_FILE_MODE);
        }

        let root = self
            .display_roots_by_environment
            .get(&path.environment_id)?;
        let relative_path = filesystem_path.strip_prefix(root).ok()?;
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "--stage", "--"])
            .arg(relative_path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        match String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()?
        {
            "100644" => Some(REGULAR_FILE_MODE),
            "100755" => Some("100755"),
            "120000" => Some("120000"),
            "160000" => Some("160000"),
            _ => None,
        }
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

#[cfg(test)]
fn post_edit_fingerprint(
    boundary: &CoherentImplementationBoundary,
    mutation_revision: u64,
    diff_identity: &str,
    dependencies: &PostEditInspectionDependencies,
) -> String {
    let canonical = format!(
        "candidate={}\nmutation={}\ndiff={}\nrequirements={}\nproof={}\nvalidation={}\ngate={}",
        boundary.candidate_identity,
        mutation_revision,
        diff_identity,
        dependencies.requirement_manifest_identity,
        dependencies.proof_route_identity,
        dependencies.validation_identity,
        dependencies.rendered_gate_identity,
    );
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn normalize_tracked_path(path: &Path) -> PathBuf {
    let lexical = lexically_normalize_path(path);
    let normalized = if lexical.is_relative() {
        lexical
    } else {
        normalize_for_path_comparison(&lexical)
            .unwrap_or_else(|_| normalize_from_existing_ancestor(&lexical).unwrap_or(lexical))
    };
    #[cfg(windows)]
    {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    normalized
}

fn normalize_from_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut normalized) = normalize_for_path_comparison(cursor) {
            for component in missing.iter().rev() {
                normalized.push(component);
            }
            return Some(normalized);
        }
        missing.push(cursor.file_name()?.to_os_string());
        cursor = cursor.parent()?;
    }
}

fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match normalized.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(std::path::Component::Prefix(_) | std::path::Component::RootDir) => {}
                Some(std::path::Component::CurDir | std::path::Component::ParentDir) | None => {
                    if !path.is_absolute() {
                        normalized.push("..");
                    }
                }
            },
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
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
        "pytest" => pytest_validation_coverage(tokens, cwd),
        "python"
            if tokens.get(1).is_some_and(|token| token == "-m")
                && tokens.get(2).is_some_and(|token| token == "pytest") =>
        {
            pytest_validation_coverage(&tokens[2..], cwd)
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
    if [
        "--test",
        "--tests",
        "--bin",
        "--bins",
        "--example",
        "--examples",
        "--lib",
        "--bench",
        "--benches",
        "--doc",
        "--exclude",
        "--no-run",
        "--exact",
        "--ignored",
        "--skip",
        "--filter-expr",
        "--run-ignored",
        "--partition",
    ]
    .iter()
    .any(|flag| has_flag(tokens, flag))
        || has_short_attached_value(tokens, "-E")
    {
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
    if let Some(package) = flag_value(tokens, &["-p", "--package"]) {
        return package_validation_coverage(package, cwd);
    }
    if matches!(recipe, "test-lane-package" | "check-lane" | "fix-lane") {
        if let Some(package) = tokens.iter().skip(2).find(|token| !token.starts_with('-')) {
            return package_validation_coverage(package, cwd);
        }
        return ValidationCoverage::ScopedUnknown;
    }
    if matches!(recipe, "test-lane" | "test-lane-fast") {
        return ValidationCoverage::ScopedUnknown;
    }
    if tokens
        .iter()
        .any(|token| matches!(token.as_str(), "-e" | "--filter-expr"))
    {
        return ValidationCoverage::ScopedUnknown;
    }
    if matches!(recipe, "test" | "test-fast" | "check") && tokens.len() == 2 {
        ValidationCoverage::All
    } else {
        ValidationCoverage::ScopedUnknown
    }
}

fn package_validation_coverage(package: &str, cwd: Option<&Path>) -> ValidationCoverage {
    let Some(cwd) = cwd else {
        return ValidationCoverage::ScopedUnknown;
    };
    find_package_directory(package, cwd)
        .map(|path| ValidationCoverage::Paths(vec![path]))
        .unwrap_or(ValidationCoverage::ScopedUnknown)
}

fn find_package_directory(package: &str, cwd: &Path) -> Option<PathBuf> {
    let mut roots = Vec::new();
    for ancestor in cwd.ancestors() {
        for candidate in [ancestor.to_path_buf(), ancestor.join("codex-rs")] {
            let manifest = candidate.join("Cargo.toml");
            let Ok(contents) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
                continue;
            };
            if value.get("workspace").is_some() {
                roots.push(candidate);
            }
        }
    }
    roots.sort();
    roots.dedup();

    for root in roots {
        if let Some(directory) = find_package_directory_in_tree(package, &root) {
            return Some(directory);
        }
    }
    None
}

fn find_package_directory_in_tree(package: &str, root: &Path) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir() {
                if !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "vendor" | "third_party" | "node_modules")
                ) {
                    pending.push(entry.path());
                }
                continue;
            }
            if !file_type.is_file() || entry.file_name() != "Cargo.toml" {
                continue;
            }
            let contents = std::fs::read_to_string(entry.path()).ok()?;
            let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
                continue;
            };
            let name = value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str);
            if name == Some(package) {
                return entry.path().parent().map(normalize_tracked_path);
            }
        }
    }
    None
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

fn pytest_validation_coverage(tokens: &[String], cwd: Option<&Path>) -> ValidationCoverage {
    if ["-k", "-m", "--lf"]
        .iter()
        .any(|flag| has_flag(tokens, flag))
        || ["--ignore", "--ignore-glob", "--deselect"]
            .iter()
            .any(|flag| has_flag(tokens, flag))
    {
        return ValidationCoverage::ScopedUnknown;
    }

    let mut positional = Vec::new();
    let mut skip_next = false;
    for token in tokens.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token.starts_with('-') {
            if !token.contains('=')
                && matches!(
                    token.as_str(),
                    "-c" | "-o"
                        | "--basetemp"
                        | "--confcutdir"
                        | "--rootdir"
                        | "--junitxml"
                        | "--override-ini"
                )
            {
                skip_next = true;
            }
            continue;
        }
        positional.push(token);
    }
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
    if let Some(value) = tokens.iter().find_map(|token| {
        flags.iter().find_map(|flag| {
            token
                .strip_prefix(flag)
                .and_then(|suffix| suffix.strip_prefix('='))
        })
    }) {
        return Some(value);
    }
    if let Some(value) = tokens.iter().find_map(|token| {
        flags
            .iter()
            .filter(|flag| flag.len() == 2)
            .find_map(|flag| {
                token
                    .strip_prefix(flag)
                    .filter(|suffix| !suffix.is_empty() && !suffix.starts_with('='))
            })
    }) {
        return Some(value);
    }
    tokens.windows(2).find_map(|window| match window {
        [flag, value] if flags.iter().any(|candidate| flag == candidate) => Some(value.as_str()),
        _ => None,
    })
}

fn has_flag(tokens: &[String], flag: &str) -> bool {
    tokens
        .iter()
        .any(|token| token == flag || token.starts_with(&format!("{flag}=")))
}

fn has_short_attached_value(tokens: &[String], flag: &str) -> bool {
    tokens.iter().any(|token| {
        token
            .get(..flag.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(flag))
            && token
                .get(flag.len()..)
                .is_some_and(|suffix| !suffix.is_empty() && !suffix.starts_with('='))
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
            command
                .iter()
                .find_map(|token| attached_filter_expression(token))
        })
        .or_else(|| {
            let tokens = shell_filter_tokens(&command.join(" "));
            tokens
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
                    tokens
                        .iter()
                        .find_map(|token| attached_filter_expression(token))
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

fn attached_filter_expression(token: &str) -> Option<String> {
    if let Some((flag, expression)) = token.split_once('=')
        && flag.eq_ignore_ascii_case("--filter-expr")
        && !expression.is_empty()
    {
        return Some(expression.to_string());
    }
    token
        .get(..2)
        .filter(|flag| flag.eq_ignore_ascii_case("-e"))
        .and_then(|_| token.get(2..))
        .filter(|expression| !expression.is_empty())
        .map(str::to_string)
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
    let format_check =
        is_format_only_command(command) && unwrapped.iter().any(|token| token == "--check");
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
    if validation_command_may_mutate(unwrapped) {
        return true;
    }
    if let Some(subcommand) = git_subcommand(unwrapped) {
        if matches!(
            subcommand,
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
        ) {
            return true;
        }
        if is_read_only_git_subcommand(subcommand) {
            return false;
        }
    }
    if format_check || codex_shell_command::is_safe_command::is_known_safe_command(command) {
        return false;
    }

    let joined = command.join(" ").to_ascii_lowercase();
    if joined.contains(">>") || joined.contains(" > ") || joined.contains("| out-file") {
        return true;
    }

    let tokens = shell_filter_tokens(&joined);
    let explicit_dry_run = tokens.iter().any(|token| token == "--dry-run");
    let short_dry_run = tokens.iter().any(|token| token == "-n");
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "chmod" | "chown" | "chgrp" | "touch" | "truncate"
        ) || (command_basename(token) == "dd" && tokens.iter().any(|arg| arg.starts_with("of=")))
            || (command_basename(token) == "patch" && !explicit_dry_run)
            || (command_basename(token) == "rsync" && !(explicit_dry_run || short_dry_run))
            || (command_basename(token) == "sed"
                && tokens.iter().any(|arg| {
                    arg == "-i"
                        || arg.starts_with("-i")
                        || arg == "--in-place"
                        || arg.starts_with("--in-place=")
                }))
            || (command_basename(token) == "perl"
                && tokens
                    .iter()
                    .any(|arg| arg.starts_with('-') && arg.trim_start_matches('-').contains('i')))
    }) {
        return true;
    }
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

    if tokens.windows(2).any(|window| match window {
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
    }) {
        return true;
    }

    if is_direct_file_read_command(command, unwrapped) || is_read_only_powershell_command(command) {
        return false;
    }

    // Mutation freshness must fail closed. The command-safety classifier above
    // proves known inspection commands read-only, and validation commands are
    // separately recognized by their exact runners. Any remaining executable
    // may be an arbitrary script or generator, so treating it as read-only can
    // leave stale source evidence live after an edit.
    !is_validation_command(command)
}

fn is_direct_file_read_command(command: &[String], unwrapped: &[String]) -> bool {
    if !matches!(unwrapped, [program, paths @ ..] if program == "type" && !paths.is_empty()) {
        return false;
    }
    let Some(program) = command.first().map(|token| command_basename(token)) else {
        return false;
    };
    if !program.eq_ignore_ascii_case("cmd") {
        return false;
    }
    !command
        .iter()
        .any(|token| token.contains(['>', '<', '&', '|', ';', '`', '$']))
}

fn is_read_only_powershell_command(command: &[String]) -> bool {
    let Some(program) = command.first().map(|token| command_basename(token)) else {
        return false;
    };
    if !matches!(program.to_ascii_lowercase().as_str(), "powershell" | "pwsh") {
        return false;
    }
    let Some(command_position) = command
        .iter()
        .position(|token| matches!(token.to_ascii_lowercase().as_str(), "-command" | "-c"))
    else {
        return false;
    };
    let script = command[command_position.saturating_add(1)..].join(" ");
    if script.trim().is_empty()
        || script.contains(['>', '<', '`', '&'])
        || script.contains("$(")
        || script.contains("::")
    {
        return false;
    }

    script
        .split([';', '|', '\n', '\r'])
        .filter(|segment| !segment.trim().is_empty())
        .all(powershell_segment_is_read_only)
}

fn powershell_segment_is_read_only(segment: &str) -> bool {
    let segment = segment
        .trim()
        .trim_matches(|ch| matches!(ch, '{' | '}'))
        .trim();
    if segment.is_empty() {
        return true;
    }
    if let Some((header, body)) = segment.split_once('{') {
        return powershell_control_header_is_read_only(header)
            && powershell_segment_is_read_only(body);
    }
    let compact = segment
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if matches!(
        compact.as_str(),
        "$erroractionpreference='stop'" | "$erroractionpreference=\"stop\""
    ) {
        return true;
    }

    if segment.starts_with('$') {
        let Some((binding, expression)) = segment.split_once('=') else {
            // Indexing, slicing, and emitting already-populated variables are
            // process-local reads. File and process invocation syntax is rejected above.
            return powershell_variable_expression_is_read_only(segment);
        };
        if !powershell_local_variable_binding_is_read_only(binding) {
            return false;
        }
        let expression = expression.trim();
        if expression.is_empty() {
            return false;
        }
        if expression.starts_with(['\'', '"'])
            || expression
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit() || ch == '-')
        {
            return true;
        }
        if expression.starts_with('$') {
            return powershell_variable_expression_is_read_only(expression);
        }
        if expression.starts_with("@(") && expression.ends_with(')') {
            return expression.chars().all(|ch| {
                ch.is_ascii_digit()
                    || ch.is_whitespace()
                    || matches!(
                        ch,
                        '@' | '$' | '(' | ')' | '[' | ']' | ',' | '.' | '-' | '\'' | '"'
                    )
            });
        }
        if expression.starts_with('(') && expression.ends_with(')') {
            return powershell_invocation_is_read_only(&expression[1..expression.len() - 1]);
        }
        return powershell_invocation_is_read_only(expression);
    }

    if matches!(
        segment
            .split_ascii_whitespace()
            .next()
            .map(|word| word.to_ascii_lowercase())
            .as_deref(),
        Some("else" | "exit")
    ) {
        return true;
    }
    powershell_invocation_is_read_only(segment)
}

fn powershell_local_variable_binding_is_read_only(binding: &str) -> bool {
    let Some(name) = binding.trim().strip_prefix('$') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn powershell_variable_expression_is_read_only(expression: &str) -> bool {
    let expression = expression.trim();
    expression.starts_with('$')
        && !expression.contains('(')
        && expression.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'$' | b'_' | b':' | b'.' | b'[' | b']' | b'-')
        })
}

fn powershell_control_header_is_read_only(header: &str) -> bool {
    let header = header.trim();
    let Some(inner) = header
        .strip_prefix("foreach")
        .or_else(|| header.strip_prefix("ForEach"))
        .map(str::trim)
        .and_then(|value| value.strip_prefix('('))
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    let Some((binding, collection)) = inner.split_once(" in ") else {
        return false;
    };
    binding.trim().starts_with('$')
        && powershell_segment_is_read_only(&format!("$collection = {}", collection.trim()))
}

fn powershell_invocation_is_read_only(invocation: &str) -> bool {
    let normalized = normalized_command_tokens(&[invocation.to_string()]);
    let Some(program) = normalized.first().map(|token| command_basename(token)) else {
        return false;
    };
    if matches!(
        program,
        "compare-object"
            | "convertfrom-json"
            | "convertto-json"
            | "format-list"
            | "format-table"
            | "get-childitem"
            | "get-command"
            | "get-content"
            | "get-date"
            | "get-item"
            | "get-location"
            | "get-member"
            | "get-variable"
            | "join-path"
            | "measure-object"
            | "out-string"
            | "resolve-path"
            | "select-object"
            | "select-string"
            | "sort-object"
            | "split-path"
            | "test-path"
            | "write-host"
            | "write-information"
            | "write-output"
            | "write-verbose"
            | "write-warning"
    ) {
        return true;
    }
    if let Some(subcommand) = git_subcommand(&normalized) {
        return is_read_only_git_subcommand(subcommand);
    }
    codex_shell_command::is_safe_command::is_known_safe_command(&normalized)
}

pub(crate) fn command_may_mutate(command: &[String]) -> bool {
    command_mutation(command, None).may_have_mutated()
}

pub(crate) fn command_mutation(command: &[String], cwd: Option<&Path>) -> CommandMutation {
    if !looks_like_mutating_command(command) {
        return CommandMutation::ReadOnly;
    }
    let paths = command_mutation_paths(command, cwd);
    if paths.is_some() || is_known_mutating_command(command) {
        CommandMutation::KnownMutation { paths }
    } else {
        CommandMutation::Uncertain
    }
}

pub(crate) fn resolve_uncertain_command_observation(
    workspace_changed: Option<bool>,
) -> CommandMutation {
    match workspace_changed {
        Some(false) => CommandMutation::ReadOnly,
        Some(true) => CommandMutation::KnownMutation { paths: None },
        None => CommandMutation::Uncertain,
    }
}

fn is_known_mutating_command(command: &[String]) -> bool {
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    if validation_command_may_mutate(unwrapped)
        || (is_format_only_command(command) && !unwrapped.iter().any(|token| token == "--check"))
        || matches!(
            unwrapped,
            [first, second, ..]
                if first == "just"
                    && matches!(second.as_str(), "fix" | "fix-lane" | "fix-workspace" | "fmt")
        )
    {
        return true;
    }
    if matches!(
        git_subcommand(unwrapped),
        Some(
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
        )
    ) {
        return true;
    }
    let tokens = shell_filter_tokens(&command.join(" ").to_ascii_lowercase());
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "apply_patch"
                | "add-content"
                | "chmod"
                | "chown"
                | "chgrp"
                | "copy-item"
                | "cp"
                | "dd"
                | "del"
                | "erase"
                | "md"
                | "mkdir"
                | "move-item"
                | "mv"
                | "new-item"
                | "ni"
                | "out-file"
                | "patch"
                | "rd"
                | "reg"
                | "remove-item"
                | "ren"
                | "rename-item"
                | "rm"
                | "rmdir"
                | "rsync"
                | "set-content"
                | "set-item"
                | "set-itemproperty"
                | "tee"
                | "tee-object"
                | "touch"
                | "truncate"
        )
    }) || command.join(" ").contains(['>', '`'])
    {
        return true;
    }
    matches!(
        unwrapped.first().map(|token| command_basename(token)),
        Some(
            "bash"
                | "cmd"
                | "node"
                | "perl"
                | "powershell"
                | "pwsh"
                | "python"
                | "python3"
                | "ruby"
                | "sh"
        )
    )
}

pub(crate) fn command_reads_repository_history(command: &[String]) -> bool {
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    matches!(git_subcommand(unwrapped), Some("log" | "show" | "shortlog"))
        && !unwrapped
            .iter()
            .any(|token| matches!(token.as_str(), "-g" | "--walk-reflogs" | "--reflog"))
}

fn validation_command_may_mutate(tokens: &[String]) -> bool {
    let tokens = if matches!(tokens, [first, second, ..] if first == "uv" && second == "run") {
        &tokens[2..]
    } else {
        tokens
    };
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return false;
    };
    if matches!(first, "npm" | "pnpm" | "yarn") && is_validation_tokens(tokens) {
        // Package scripts are arbitrary programs even when their names contain
        // `test`, `lint`, or `build`; they may update snapshots or generated files.
        return true;
    }
    if first == "eslint"
        && tokens
            .iter()
            .any(|token| token == "--fix" || token.starts_with("--fix="))
    {
        return true;
    }
    if first == "ruff"
        && tokens.iter().any(|token| {
            matches!(token.as_str(), "--fix" | "--fix-only") || token.starts_with("--fix=")
        })
    {
        return true;
    }
    matches!(first, "jest" | "vitest" | "playwright" | "pytest")
        && tokens.iter().any(|token| {
            token == "-u"
                || matches!(
                    token.as_str(),
                    "--update-snapshot"
                        | "--update-snapshots"
                        | "--updatesnapshot"
                        | "--snapshot-update"
                )
                || (token.contains("snapshot") && token.contains("update"))
        })
}

fn git_subcommand(tokens: &[String]) -> Option<&str> {
    let git = tokens
        .iter()
        .position(|token| command_basename(token) == "git")?;
    let mut index = git.saturating_add(1);
    while let Some(token) = tokens.get(index).map(String::as_str) {
        if matches!(
            token,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
        ) {
            index = index.saturating_add(2);
            continue;
        }
        if matches!(
            token,
            "--no-pager"
                | "--paginate"
                | "-p"
                | "--literal-pathspecs"
                | "--no-literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
        ) || token.starts_with("--git-dir=")
            || token.starts_with("--work-tree=")
            || token.starts_with("--namespace=")
            || token.starts_with("--exec-path=")
            || token.starts_with("--config-env=")
        {
            index = index.saturating_add(1);
            continue;
        }
        return (!token.starts_with('-')).then_some(token);
    }
    None
}

fn is_read_only_git_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "cat-file"
            | "describe"
            | "diff"
            | "for-each-ref"
            | "grep"
            | "log"
            | "ls-files"
            | "ls-tree"
            | "name-rev"
            | "rev-parse"
            | "shortlog"
            | "show"
            | "status"
    )
}

/// Returns exact paths only for simple, direct mutators whose operands can be
/// interpreted without shell expansion. Complex scripts deliberately fall back
/// to unknown mutation invalidation.
pub(crate) fn command_mutation_paths(
    command: &[String],
    cwd: Option<&Path>,
) -> Option<BTreeSet<PathBuf>> {
    if !looks_like_mutating_command(command) || command.is_empty() {
        return None;
    }
    let program = command_basename(&command[0].to_ascii_lowercase()).to_string();
    let args = &command[1..];
    if args.iter().any(|arg| {
        arg.contains(['*', '?', '|', '>', '<', ';', '&', '$', '`']) || arg.starts_with('@')
    }) {
        return None;
    }

    let operands = match program.as_str() {
        "touch" | "truncate" | "mkdir" | "md" | "rmdir" | "rd" | "rm" | "del" | "erase" => {
            simple_path_operands(args)?
        }
        "cp" | "mv" => {
            let paths = simple_path_operands(args)?;
            (paths.len() >= 2).then_some(paths)?
        }
        "chmod" | "chown" | "chgrp" => {
            let operands = simple_path_operands(args)?;
            (operands.len() >= 2).then(|| operands.into_iter().skip(1).collect())?
        }
        "rustfmt" | "prettier" if !args.iter().any(|arg| arg == "--check") => {
            simple_path_operands(args)?
        }
        _ => return None,
    };
    let paths = operands
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                Some(path)
            } else {
                cwd.map(|cwd| cwd.join(path))
            }
        })
        .collect::<Option<BTreeSet<_>>>()?;
    (!paths.is_empty()).then_some(paths)
}

fn simple_path_operands(args: &[String]) -> Option<Vec<String>> {
    let mut operands = Vec::new();
    let mut options_ended = false;
    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && arg.starts_with('-') {
            if !matches!(
                arg.as_str(),
                "-f" | "--force"
                    | "-r"
                    | "-R"
                    | "--recursive"
                    | "-p"
                    | "--parents"
                    | "-v"
                    | "--verbose"
            ) {
                return None;
            }
        } else {
            operands.push(arg.clone());
        }
    }
    Some(operands)
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
    let basename = token.rsplit(['/', '\\']).next().unwrap_or(token);
    basename.strip_suffix(".exe").unwrap_or(basename)
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
