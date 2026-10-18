use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use sha1::digest::Output;

use codex_apply_patch::AppliedPatchChange;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
pub(crate) use codex_command_analysis::command_mutation::CommandMutation;
pub(crate) use codex_command_analysis::command_mutation::command_is_read_only_git;
#[cfg(test)]
use codex_command_analysis::command_mutation::command_may_mutate;
pub(crate) use codex_command_analysis::command_mutation::command_mutation;
#[cfg(test)]
use codex_command_analysis::command_mutation::command_mutation_paths;
pub(crate) use codex_command_analysis::command_mutation::command_reads_repository_history;
pub(crate) use codex_command_analysis::command_mutation::resolve_uncertain_command_observation;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DEV_NULL: &str = "/dev/null";
const REGULAR_FILE_MODE: &str = "100644";
// Normal edits finish well within 100 ms; pathological inputs fall back to a coarse,
// content-exact diff without stalling tool completion.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const TURN_DIFF_LOCATOR: &str = "context:turn-diff";
struct TrackedContent {
    content: String,
    mode: Option<String>,
    blob_oid: OnceCell<String>,
}

#[derive(Clone, Debug)]
struct TrackedPath {
    environment_id: String,
    path: PathBuf,
    comparison_key: PathBuf,
}

impl TrackedPath {
    fn new(environment_id: &str, path: &Path) -> Self {
        let path = normalize_tracked_path(path);
        #[cfg(windows)]
        let comparison_key = PathBuf::from(windows_path_comparison_key(path.as_os_str()));
        #[cfg(not(windows))]
        let comparison_key = path.clone();
        Self {
            environment_id: environment_id.to_string(),
            path,
            comparison_key,
        }
    }
}

impl PartialEq for TrackedPath {
    fn eq(&self, other: &Self) -> bool {
        self.environment_id == other.environment_id && self.comparison_key == other.comparison_key
    }
}

impl Eq for TrackedPath {}

impl Hash for TrackedPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.environment_id.hash(state);
        self.comparison_key.hash(state);
    }
}

impl PartialOrd for TrackedPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TrackedPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.environment_id, &self.comparison_key)
            .cmp(&(&other.environment_id, &other.comparison_key))
    }
}

/// Tracks the net text diff for the current turn from committed apply_patch
/// mutations. Unattributed changes recheck only these tracked paths.
pub struct TurnDiffTracker {
    valid: bool,
    invalidation_reported: bool,
    first_invalidation_cause: Option<serde_json::Value>,
    display_roots_by_environment: HashMap<String, PathBuf>,
    baseline_by_path: HashMap<TrackedPath, TrackedContent>,
    current_by_path: HashMap<TrackedPath, TrackedContent>,
    origin_by_current_path: HashMap<TrackedPath, TrackedPath>,
    mutation_revision: u64,
    attributed_mutation_revision: u64,
    /// Exact patch effects since each validation baseline. Unknown effects
    /// invalidate the proof without discarding the ordinary net diff.
    validation_path_revisions: HashMap<TrackedPath, u64>,
    validation_unknown_revision: u64,
    rendered_diffs: BTreeMap<(String, TrackedPath), String>,
    unavailable_paths: BTreeSet<TrackedPath>,
    /// Observed workspace changes that no command declared and no tracked path
    /// explains: another writer's, or an undeclared command effect. They are
    /// reported, never charged to this turn's change set.
    unattributed_paths: BTreeSet<TrackedPath>,
    display_keys: HashMap<TrackedPath, String>,
    destination_by_origin: HashMap<TrackedPath, TrackedPath>,
    aggregate_dirty: bool,
    patch_modes: HashMap<TrackedPath, String>,
    unified_diff: Option<String>,
    last_emitted_unified_diff: Option<String>,
    workspace_evidence_generation_batch:
        Option<Weak<crate::tools::parallel::WorkspaceEvidenceGenerationBatch>>,
    #[cfg(test)]
    rendered_diff_count: std::cell::Cell<usize>,
}

impl Default for TurnDiffTracker {
    fn default() -> Self {
        Self {
            valid: true,
            invalidation_reported: false,
            first_invalidation_cause: None,
            display_roots_by_environment: HashMap::new(),
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            origin_by_current_path: HashMap::new(),
            mutation_revision: 0,
            attributed_mutation_revision: 0,
            validation_path_revisions: HashMap::new(),
            validation_unknown_revision: 0,
            rendered_diffs: BTreeMap::new(),
            unavailable_paths: BTreeSet::new(),
            unattributed_paths: BTreeSet::new(),
            display_keys: HashMap::new(),
            destination_by_origin: HashMap::new(),
            aggregate_dirty: false,
            patch_modes: HashMap::new(),
            unified_diff: None,
            last_emitted_unified_diff: None,
            workspace_evidence_generation_batch: None,
            #[cfg(test)]
            rendered_diff_count: std::cell::Cell::new(0),
        }
    }
}

impl TurnDiffTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn activate_workspace_evidence_generation_batch(
        &mut self,
        batch: &Arc<crate::tools::parallel::WorkspaceEvidenceGenerationBatch>,
    ) {
        self.workspace_evidence_generation_batch = Some(Arc::downgrade(batch));
    }

    pub(crate) fn workspace_evidence_generation_batch_for_call(
        &self,
        call_id: &str,
    ) -> Option<Arc<crate::tools::parallel::WorkspaceEvidenceGenerationBatch>> {
        self.workspace_evidence_generation_batch
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|batch| batch.accepts_call(call_id))
    }

    pub(crate) fn clear_workspace_evidence_generation_batch(
        &mut self,
        batch: &Arc<crate::tools::parallel::WorkspaceEvidenceGenerationBatch>,
    ) {
        let active_is_batch = self
            .workspace_evidence_generation_batch
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|active| Arc::ptr_eq(&active, batch));
        if active_is_batch {
            self.workspace_evidence_generation_batch = None;
        }
    }

    pub fn with_environment_display_roots(
        display_roots: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> Self {
        let mut tracker = Self::new();
        tracker.set_environment_display_roots(display_roots);
        tracker
    }

    pub(crate) fn set_environment_display_roots(
        &mut self,
        display_roots: impl IntoIterator<Item = (String, PathBuf)>,
    ) {
        self.display_roots_by_environment = display_roots
            .into_iter()
            .map(|(environment_id, root)| (environment_id, normalize_tracked_path(&root)))
            .collect();
    }

    pub fn track_delta(&mut self, environment_id: &str, delta: &AppliedPatchDelta) {
        let previous_unknown_revision = self.validation_unknown_revision;
        if !delta.is_empty() {
            self.record_mutation();
        }

        if !self.valid {
            return;
        }

        if !delta.is_exact() {
            self.record_invalidation_cause(serde_json::json!({
                "tool": "apply_patch", "mutation_class": "inexact_patch_delta",
                "environment_id": environment_id,
            }));
            self.invalidate();
            return;
        }

        self.validation_unknown_revision = previous_unknown_revision;

        let mut changed = HashSet::new();
        for change in delta.changes() {
            let mut paths = vec![TrackedPath::new(environment_id, &change.path)];
            if let AppliedPatchFileChange::Update {
                move_path: Some(path),
                ..
            } = &change.change
            {
                paths.push(TrackedPath::new(environment_id, path));
            }
            for path in &paths {
                self.validation_path_revisions
                    .insert(path.clone(), self.mutation_revision);
                self.collect_diff_partners(path, &mut changed);
            }
            if paths
                .iter()
                .any(|path| self.unavailable_paths.contains(path))
            {
                self.unavailable_paths.extend(paths.iter().cloned());
            }
            self.apply_change(environment_id, change);
            for path in &paths {
                self.collect_diff_partners(path, &mut changed);
            }
        }
        self.refresh_unified_diff(changed);
        self.patch_modes.clear();
    }

    pub(crate) fn missing_mode_paths(
        &self,
        environment_id: &str,
        delta: &AppliedPatchDelta,
    ) -> Vec<PathBuf> {
        if !self.valid || !delta.is_exact() {
            return Vec::new();
        }
        delta
            .changes()
            .iter()
            .filter_map(|change| {
                if !matches!(
                    &change.change,
                    AppliedPatchFileChange::Delete { .. }
                        | AppliedPatchFileChange::Update {
                            move_path: Some(_),
                            ..
                        }
                ) {
                    return None;
                }
                let path = TrackedPath::new(environment_id, &change.path);
                if self.current_by_path.contains_key(&path)
                    || self.baseline_by_path.contains_key(&path)
                    || std::fs::symlink_metadata(&change.path).is_ok()
                {
                    return None;
                }
                Some(change.path.clone())
            })
            .collect()
    }

    pub(crate) fn set_patch_modes(
        &mut self,
        environment_id: &str,
        modes: HashMap<PathBuf, String>,
    ) {
        self.patch_modes = modes
            .into_iter()
            .map(|(path, mode)| (TrackedPath::new(environment_id, &path), mode))
            .collect();
    }

    pub(crate) fn patch_mode_root(&self, environment_id: &str) -> Option<&Path> {
        self.display_roots_by_environment
            .get(environment_id)
            .map(PathBuf::as_path)
    }

    pub fn invalidate(&mut self) {
        self.record_invalidation_cause(serde_json::json!({
            "mutation_class": "unknown_mutation",
        }));
        self.valid = false;
        self.rendered_diffs.clear();
        self.aggregate_dirty = false;
        self.unified_diff = None;
    }

    pub(crate) fn record_unknown_mutation(&mut self) {
        self.record_mutation();
        self.invalidate();
    }

    pub(crate) fn record_invalidation_cause(&mut self, cause: serde_json::Value) {
        self.first_invalidation_cause.get_or_insert(cause);
    }

    pub(crate) fn record_command_invalidation_cause(
        &mut self,
        call_id: Option<&str>,
        tool: &str,
        command: &[String],
        mutation: &CommandMutation,
    ) {
        if self.first_invalidation_cause.is_some() {
            return;
        }
        let mutation_class = match mutation {
            CommandMutation::ReadOnly => return,
            CommandMutation::KnownMutation { paths: Some(paths) } if paths.is_empty() => return,
            CommandMutation::KnownMutation { .. } => "known_mutation",
            CommandMutation::Uncertain => "uncertain",
            CommandMutation::UnattributedWorkspaceChange => "unattributed_workspace_change",
        };
        let command_prefix: String = command
            .iter()
            .enumerate()
            .flat_map(|(index, arg)| (index != 0).then_some(' ').into_iter().chain(arg.chars()))
            .take(240)
            .collect();
        self.record_invalidation_cause(serde_json::json!({
            "call_id": call_id, "tool": tool, "mutation_class": mutation_class,
            "command_prefix": command_prefix,
        }));
    }

    #[cfg(test)]
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
        _exit_code: i32,
        _timed_out: bool,
        environment_id: &str,
        cwd: Option<&Path>,
        mutation: CommandMutation,
    ) {
        // A command can write before failing or timing out, so every observed
        // mutation advances the generic turn revision. Known touched paths
        // lose exactness without discarding unrelated apply_patch diffs.
        self.record_command_invalidation_cause(None, "shell", command, &mutation);
        match mutation {
            CommandMutation::KnownMutation { paths: Some(paths) } => {
                if paths.is_empty() {
                    self.mutation_revision = self.mutation_revision.saturating_add(1);
                    self.validation_unknown_revision = self.mutation_revision;
                } else {
                    self.record_mutation();
                }
                if !self.valid {
                    return;
                }
                let mut affected = HashSet::new();
                for path in paths {
                    let absolute = if path.is_absolute() {
                        path
                    } else {
                        cwd.map_or_else(|| path.clone(), |cwd| cwd.join(&path))
                    };
                    let path = TrackedPath::new(environment_id, &absolute);
                    self.collect_diff_partners(&path, &mut affected);
                    for tracked in self
                        .baseline_by_path
                        .keys()
                        .chain(self.current_by_path.keys())
                    {
                        if tracked.environment_id == path.environment_id
                            && tracked.comparison_key.starts_with(&path.comparison_key)
                        {
                            self.collect_diff_partners(tracked, &mut affected);
                        }
                    }
                }
                self.rendered_diffs
                    .retain(|(_, path), _| !affected.contains(path));
                self.unavailable_paths.extend(affected);
                self.aggregate_dirty = true;
            }
            CommandMutation::KnownMutation { paths: None } => {
                self.record_unknown_mutation();
            }
            CommandMutation::Uncertain | CommandMutation::UnattributedWorkspaceChange => {
                // This revision invalidates cached evidence; it is not proof
                // that this command wrote files or that the task is an edit.
                self.mutation_revision = self.mutation_revision.saturating_add(1);
                self.invalidate();
            }
            CommandMutation::ReadOnly => {}
        }
    }

    /// Narrow uncertain effects to mismatching patch paths. The caller holds
    /// the tracker lock through observation and recording, so a concurrent
    /// patch cannot install newer expected content midway through this check.
    pub(crate) async fn reconcile_command_mutation(
        &mut self,
        environment_id: &str,
        mutation: CommandMutation,
        fs: Option<&dyn codex_exec_server::ExecutorFileSystem>,
    ) -> CommandMutation {
        if !self.valid
            || !matches!(
                mutation,
                CommandMutation::Uncertain | CommandMutation::UnattributedWorkspaceChange
            )
        {
            return mutation;
        }
        let paths = self
            .baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .filter(|path| path.environment_id == environment_id)
            .collect::<BTreeSet<_>>();
        let mut mismatches = BTreeSet::new();
        for path in paths {
            let expected = self
                .current_by_path
                .get(path)
                .map(|content| content.content.as_str());
            if !content_matches(&path.path, expected, fs).await {
                mismatches.insert(path.path.clone());
            }
        }
        // Even an empty mismatch set advances the workspace evidence revision;
        // it does not turn an unattributed change into proof of a read-only call.
        CommandMutation::KnownMutation {
            paths: Some(mismatches),
        }
    }

    /// Attributes a command's observed workspace changes to this turn: its
    /// declared paths, plus observed tracked paths whose content no longer
    /// matches this turn's edits. Any other observed path, such as a concurrent
    /// session's write, is kept as unattributed rather than discarding this
    /// turn's exact diffs or entering its change set. The caller holds the
    /// tracker lock, as for `reconcile_command_mutation`.
    pub(crate) async fn attribute_observed_changes(
        &mut self,
        environment_id: &str,
        cwd: Option<&Path>,
        declared: Option<&BTreeSet<PathBuf>>,
        observed: &BTreeSet<PathBuf>,
        fs: Option<&dyn codex_exec_server::ExecutorFileSystem>,
    ) -> CommandMutation {
        let mut attributed = declared
            .into_iter()
            .flatten()
            .map(|path| match cwd {
                Some(cwd) if !path.is_absolute() => cwd.join(path),
                _ => path.clone(),
            })
            .collect::<BTreeSet<_>>();
        let declared = attributed
            .iter()
            .map(|path| TrackedPath::new(environment_id, path))
            .collect::<Vec<_>>();
        let mut unattributed = Vec::new();
        for path in observed {
            let observed_path = TrackedPath::new(environment_id, path);
            if declared.iter().any(|declared| {
                observed_path
                    .comparison_key
                    .starts_with(&declared.comparison_key)
            }) {
                continue;
            }
            let tracked = self.baseline_by_path.contains_key(&observed_path)
                || self.current_by_path.contains_key(&observed_path);
            if !tracked {
                unattributed.push(observed_path);
                continue;
            }
            let expected = self
                .current_by_path
                .get(&observed_path)
                .map(|content| content.content.as_str());
            if !content_matches(&observed_path.path, expected, fs).await {
                attributed.insert(path.clone());
            }
        }
        self.unattributed_paths.extend(unattributed);
        CommandMutation::KnownMutation {
            paths: Some(attributed),
        }
    }

    /// Display paths of observed changes this turn neither made nor declared.
    pub(crate) fn unattributed_display_paths(&self) -> Vec<String> {
        self.unattributed_paths
            .iter()
            .map(|path| self.display_path(path))
            .collect()
    }

    /// Display paths of files that existed before this turn and that it
    /// modified or deleted, following a rename to its destination. Unlike an
    /// added file, such an edit can change what an existing check asserts.
    #[cfg(test)]
    pub(crate) fn changed_existing_display_paths(&self) -> Vec<String> {
        self.changed_existing_contents()
            .into_iter()
            .map(|(path, ..)| path)
            .collect()
    }

    /// Files that existed before this turn and that it modified or deleted,
    /// sorted by display path, with their text before the turn and after it
    /// (`None` once deleted), following a rename to its destination.
    pub(crate) fn changed_existing_contents(&self) -> Vec<(String, String, Option<String>)> {
        self.baseline_by_path
            .iter()
            .filter_map(|(path, before)| {
                let after = self.current_by_path.get(path).or_else(|| {
                    self.rename_destination(path)
                        .and_then(|destination| self.current_by_path.get(destination))
                });
                after
                    .is_none_or(|after| after.content != before.content)
                    .then(|| {
                        (
                            self.display_path(path),
                            (
                                before.content.clone(),
                                after.map(|after| after.content.clone()),
                            ),
                        )
                    })
            })
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(path, (before, after))| (path, before, after))
            .collect()
    }

    pub(crate) fn current_mutation_revision(&self) -> u64 {
        self.mutation_revision
    }

    pub(crate) fn validation_changes_since(&self, revision: u64) -> Option<Vec<(String, PathBuf)>> {
        if !self.valid
            || self.validation_unknown_revision > revision
            || revision > self.mutation_revision
        {
            return None;
        }
        Some(
            self.validation_path_revisions
                .iter()
                .filter(|(_, changed_at)| **changed_at > revision)
                .map(|(path, _)| (path.environment_id.clone(), path.path.clone()))
                .collect(),
        )
    }

    pub(crate) fn attributed_mutation_revision(&self) -> u64 {
        self.attributed_mutation_revision
    }

    pub(crate) fn has_untracked_changes(&self) -> bool {
        !self.valid || !self.unavailable_paths.is_empty()
    }

    /// Known patch paths remain useful for validation attribution even when
    /// their exact diff is unavailable. This is not the entire workspace delta.
    pub(crate) fn changed_paths(&self) -> Vec<(String, PathBuf)> {
        self.baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .chain(self.unavailable_paths.iter())
            .filter(|path| {
                !self.valid
                    || self.unavailable_paths.contains(*path)
                    || match (
                        self.baseline_by_path.get(*path),
                        self.current_by_path.get(*path),
                    ) {
                        (Some(before), Some(after)) => {
                            before.content != after.content || before.mode != after.mode
                        }
                        (None, None) => false,
                        _ => true,
                    }
            })
            .map(|path| (path.environment_id.clone(), path.path.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[cfg(test)]
    pub fn get_unified_diff(&self) -> Option<String> {
        if self.aggregate_dirty {
            self.flatten_diff()
        } else {
            self.unified_diff.clone()
        }
    }

    /// Returns the net diff of this turn's tracked edits when it is exact, or
    /// the display paths whose changes it cannot diff. An empty list means
    /// command effects invalidated the whole diff. Does not affect publication.
    pub(crate) fn exact_unified_diff(&self) -> Result<Option<String>, Vec<String>> {
        if !self.valid {
            return Err(Vec::new());
        }
        if !self.unavailable_paths.is_empty() {
            return Err(self
                .unavailable_paths
                .iter()
                .map(|path| self.display_path(path))
                .collect());
        }
        let diff = if self.aggregate_dirty {
            self.flatten_diff()
        } else {
            self.unified_diff.clone()
        };
        Ok(diff.filter(|diff| !diff.is_empty()))
    }

    /// Returns the latest aggregate only when it differs from the last value
    /// returned by this method. An empty string represents a previously
    /// published diff being cleared. Publish `take_invalidation_warning` too:
    /// clearing an invalidated diff does not mean the workspace is unchanged.
    pub fn take_unified_diff_if_changed(&mut self) -> Option<String> {
        if self.aggregate_dirty {
            self.unified_diff = self.flatten_diff();
            self.aggregate_dirty = false;
        }
        if self.unified_diff == self.last_emitted_unified_diff {
            return None;
        }
        self.last_emitted_unified_diff
            .clone_from(&self.unified_diff);
        Some(self.unified_diff.clone().unwrap_or_default())
    }

    pub(crate) fn take_invalidation_warning(&mut self) -> Option<String> {
        if (self.valid && self.unavailable_paths.is_empty()) || self.invalidation_reported {
            return None;
        }
        self.invalidation_reported = true;
        let status = if self.valid { "partial" } else { "unavailable" };
        Some(format!(
            "The turn diff is {status} because command effects or workspace changes could not be tracked exactly. First cause: {}. Do not claim that no files changed without fresh workspace verification.",
            self.first_invalidation_cause
                .as_ref()
                .unwrap_or(&serde_json::Value::Null),
        ))
    }

    fn record_mutation(&mut self) {
        self.mutation_revision = self.mutation_revision.saturating_add(1);
        self.validation_unknown_revision = self.mutation_revision;
        self.attributed_mutation_revision = self.attributed_mutation_revision.saturating_add(1);
    }

    fn collect_diff_partners(&self, path: &TrackedPath, changed: &mut HashSet<TrackedPath>) {
        changed.insert(path.clone());
        if let Some(origin) = self.origin_by_current_path.get(path) {
            changed.insert(origin.clone());
        }
        if let Some(destination) = self.destination_by_origin.get(path) {
            changed.insert(destination.clone());
        }
    }

    fn flatten_diff(&self) -> Option<String> {
        if self.rendered_diffs.is_empty() && self.unavailable_paths.is_empty() {
            return None;
        }
        let mut aggregate =
            String::with_capacity(self.rendered_diffs.values().map(String::len).sum());
        for fragment in self.rendered_diffs.values() {
            aggregate.push_str(fragment);
        }
        for path in &self.unavailable_paths {
            let _ = writeln!(
                aggregate,
                "# {}: changed, diff unavailable (command effects)",
                self.display_path(path)
            );
        }
        Some(aggregate)
    }

    fn refresh_unified_diff(&mut self, changed: HashSet<TrackedPath>) {
        if changed.is_empty() {
            return;
        }
        for path in changed {
            if self.unavailable_paths.contains(&path) {
                self.rendered_diffs
                    .retain(|(_, rendered_path), _| rendered_path != &path);
                continue;
            }
            let display = match self.display_keys.get(&path) {
                Some(display) => display.clone(),
                None => {
                    let display = self.display_path(&path);
                    self.display_keys.insert(path.clone(), display.clone());
                    display
                }
            };
            let paired_destination = self
                .origin_by_current_path
                .get(&path)
                .and_then(|origin| self.rename_destination(origin))
                .is_some();
            if paired_destination {
                self.rendered_diffs.remove(&(display, path));
                continue;
            }
            let right_path = self.rename_destination(&path).unwrap_or(&path);
            let rendered = self.render_diff(
                &path,
                self.baseline_by_path.get(&path),
                right_path,
                self.current_by_path.get(right_path),
            );
            let key = (display, path);
            match rendered {
                Some(mut fragment) => {
                    if !fragment.ends_with('\n') {
                        fragment.push('\n');
                    }
                    self.rendered_diffs.insert(key, fragment);
                }
                None => {
                    self.rendered_diffs.remove(&key);
                }
            }
        }
        self.aggregate_dirty = true;
    }

    fn remove_origin(&mut self, path: &TrackedPath) -> Option<TrackedPath> {
        let origin = self.origin_by_current_path.remove(path)?;
        self.destination_by_origin.remove(&origin);
        Some(origin)
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
        self.remove_origin(&path);
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
        self.remove_origin(&path);
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
                    .remove_origin(&source_path)
                    .unwrap_or_else(|| source_path.clone());
                self.current_by_path.remove(&source_path);
                let new_content = self.tracked_content(&dest_path, new_content);
                self.current_by_path.insert(dest_path.clone(), new_content);
                self.remove_origin(&dest_path);
                if dest_path != origin {
                    self.destination_by_origin
                        .insert(origin.clone(), dest_path.clone());
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
        TrackedContent {
            content: content.to_string(),
            mode,
            blob_oid: OnceCell::new(),
        }
    }

    fn rename_destination(&self, origin: &TrackedPath) -> Option<&TrackedPath> {
        let destination = self.destination_by_origin.get(origin)?;
        (!self.current_by_path.contains_key(origin)
            && self.current_by_path.contains_key(destination)
            && self.baseline_by_path.contains_key(origin)
            && !self.baseline_by_path.contains_key(destination))
        .then_some(destination)
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
        if left_text == right_text && (left_path == right_path || left_text.is_none()) {
            return None;
        }

        #[cfg(test)]
        self.rendered_diff_count
            .set(self.rendered_diff_count.get() + 1);

        let left_display = self.display_path(left_path);
        let right_display = self.display_path(right_path);
        if left_text == right_text {
            // A content-preserving move still changes the workspace. Include
            // Git's rename metadata so exact exports can replay it.
            return Some(format!(
                "diff --git a/{left_display} b/{right_display}\nsimilarity index 100%\nrename from {left_display}\nrename to {right_display}\n"
            ));
        }
        let left_oid = left_content.map_or(ZERO_OID, |content| {
            content
                .blob_oid
                .get_or_init(|| git_blob_oid(content.content.as_bytes()))
                .as_str()
        });
        let right_oid = right_content.map_or(ZERO_OID, |content| {
            content
                .blob_oid
                .get_or_init(|| git_blob_oid(content.content.as_bytes()))
                .as_str()
        });
        let mut diff = format!("diff --git a/{left_display} b/{right_display}\n");
        match (left_content, right_content) {
            (None, Some(_)) => {
                if let Some(mode) = right_content
                    .and_then(|content| content.mode.as_deref())
                    .or_else(|| self.file_mode(right_path))
                {
                    let _ = writeln!(diff, "new file mode {mode}");
                }
            }
            (Some(_), None) => {
                if let Some(mode) = left_content
                    .and_then(|content| content.mode.as_deref())
                    .or_else(|| self.file_mode(left_path))
                {
                    let _ = writeln!(diff, "deleted file mode {mode}");
                }
            }
            (Some(_), Some(_)) => {}
            (None, None) => return None,
        }

        let _ = writeln!(diff, "index {left_oid}..{right_oid}");

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
        let changes = config.diff_lines(left_text.unwrap_or(""), right_text.unwrap_or(""));
        let mut unified = changes.unified_diff();
        let unified = unified.context_radius(3).header(&old_header, &new_header);
        let _ = write!(diff, "{unified}");
        Some(diff)
    }

    fn file_mode(&self, path: &TrackedPath) -> Option<&str> {
        if let Some(mode) = self.patch_modes.get(path) {
            return Some(mode.as_str());
        }
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
                if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                    return Some("100755");
                }
            }
            return Some(REGULAR_FILE_MODE);
        }

        None
    }

    #[cfg(test)]
    fn rendered_diff_count(&self) -> usize {
        self.rendered_diff_count.get()
    }

    fn display_path(&self, path: &TrackedPath) -> String {
        let display = self
            .display_roots_by_environment
            .get(&path.environment_id)
            .and_then(|root| strip_tracked_root(&path.path, root))
            .unwrap_or(path.path.as_path());
        let display = display.to_string_lossy().replace('\\', "/");
        if self.display_roots_by_environment.len() > 1 && !path.environment_id.is_empty() {
            format!("{}/{display}", path.environment_id)
        } else {
            display
        }
    }
}

#[cfg(test)]
thread_local! {
    static PATCH_MODE_QUERY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GIT_BLOB_HASH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) async fn resolve_patch_index_modes(
    root: &Path,
    paths: Vec<PathBuf>,
) -> HashMap<PathBuf, String> {
    let root = normalize_tracked_path(root);
    let paths = paths
        .into_iter()
        .filter_map(|path| {
            let path = normalize_tracked_path(&path);
            strip_tracked_root(&path, &root).map(Path::to_path_buf)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut modes = HashMap::new();
    // Keep both the path count and command-line size bounded on Windows.
    let mut start = 0;
    while start < paths.len() {
        let mut end = start;
        let mut length = 0;
        while end < paths.len() && end - start < 64 {
            let next = paths[end].as_os_str().len() + 3;
            if end > start && length + next > 16_000 {
                break;
            }
            length += next;
            end += 1;
        }
        #[cfg(test)]
        PATCH_MODE_QUERY_COUNT.with(|count| count.set(count.get() + 1));
        if let Some(output) = codex_git_utils::git_index_entries(&root, &paths[start..end]).await {
            for record in output.split(|byte| *byte == 0) {
                let Some(tab) = memchr::memchr(b'\t', record) else {
                    continue;
                };
                let fields = record[..tab]
                    .split(|byte| *byte == b' ')
                    .collect::<Vec<_>>();
                if fields.len() != 3 || fields[2] != b"0" {
                    continue;
                }
                let mode = match fields[0] {
                    b"100644" => "100644",
                    b"100755" => "100755",
                    b"120000" => "120000",
                    b"160000" => "160000",
                    _ => continue,
                };
                #[cfg(unix)]
                let path = {
                    use std::os::unix::ffi::OsStrExt;
                    PathBuf::from(std::ffi::OsStr::from_bytes(&record[tab + 1..]))
                };
                #[cfg(not(unix))]
                let path = match std::str::from_utf8(&record[tab + 1..]) {
                    Ok(path) => PathBuf::from(path),
                    Err(_) => continue,
                };
                modes.insert(root.join(path), mode.to_owned());
            }
        }
        start = end;
    }
    modes
}

fn normalize_tracked_path(path: &Path) -> PathBuf {
    lexically_normalize_path(dunce::simplified(path))
}

#[cfg(windows)]
fn windows_path_comparison_key(path: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;
    // Retain Windows case aliases without converting invalid native units to U+FFFD.
    let mut folded = Vec::new();
    for character in char::decode_utf16(path.encode_wide()) {
        match character {
            Ok(character) => {
                for lower in character.to_lowercase() {
                    folded.extend_from_slice(lower.encode_utf16(&mut [0; 2]));
                }
            }
            Err(error) => folded.push(error.unpaired_surrogate()),
        }
    }
    std::ffi::OsString::from_wide(&folded)
}

fn strip_tracked_root<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(relative);
    }
    #[cfg(windows)]
    {
        let mut components = path.components();
        for root_component in root.components() {
            if windows_path_comparison_key(components.next()?.as_os_str())
                != windows_path_comparison_key(root_component.as_os_str())
            {
                return None;
            }
        }
        Some(components.as_path())
    }
    #[cfg(not(windows))]
    None
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

/// Whether a tracked path still holds this turn's expected content (`None`:
/// deleted). Without a readable filesystem the content cannot be proven.
async fn content_matches(
    path: &Path,
    expected: Option<&str>,
    fs: Option<&dyn codex_exec_server::ExecutorFileSystem>,
) -> bool {
    let (Some(fs), Ok(uri)) = (
        fs,
        codex_utils_path_uri::PathUri::from_host_native_path(path),
    ) else {
        return false;
    };
    let limit = expected.map_or(0, str::len);
    match (expected, fs.read_file_bounded(&uri, limit, None).await) {
        (Some(expected), Ok(Some(actual))) => actual == expected.as_bytes(),
        (None, Err(error)) => error.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

fn git_blob_oid(data: &[u8]) -> String {
    #[cfg(test)]
    GIT_BLOB_HASH_COUNT.with(|count| count.set(count.get() + 1));
    hex::encode(git_blob_sha1_hex_bytes(data))
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
