use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
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
    comparison_key: String,
}

impl TrackedPath {
    fn new(environment_id: &str, path: &Path) -> Self {
        // Endpoint spelling comes from the executor. Host Windows case and
        // separator rules cannot define identity for remote POSIX filenames.
        let comparison_key = hex::encode(path.as_os_str().as_encoded_bytes());
        let path = path.to_path_buf();
        Self {
            environment_id: environment_id.to_string(),
            path,
            comparison_key,
        }
    }

    fn is_within(&self, root: &Self, windows: bool) -> bool {
        if self.environment_id != root.environment_id { return false; }
        let Some(rest) = self.comparison_key.strip_prefix(&root.comparison_key) else {
            return false;
        };
        rest.is_empty() || rest.starts_with("2f") || root.comparison_key.ends_with("2f")
            || (windows && (rest.starts_with("5c") || root.comparison_key.ends_with("5c")))
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

struct OwnedPatchOperation {
    sequence: u64,
    environment_id: String,
    call_id: Option<String>,
    delta: AppliedPatchDelta,
    retained_bytes: usize,
}

/// Tracks the net text diff for the current turn from committed apply_patch
/// mutations. Unattributed changes recheck only these tracked paths.
pub struct TurnDiffTracker {
    valid: bool,
    invalidation_reported: bool,
    first_invalidation_cause: Option<serde_json::Value>,
    invalidation_causes: Vec<serde_json::Value>,
    owned_operations: VecDeque<OwnedPatchOperation>,
    next_owned_operation: u64,
    owned_operation_bytes: usize,
    owned_operations_complete: bool,
    display_roots_by_environment: HashMap<String, PathBuf>,
    remote_environments: HashSet<String>,
    baseline_by_path: HashMap<TrackedPath, TrackedContent>,
    current_by_path: HashMap<TrackedPath, TrackedContent>,
    origin_by_current_path: HashMap<TrackedPath, TrackedPath>,
    mutation_revision: u64,
    attributed_mutation_revision: u64,
    /// Exact patch effects since each validation baseline, in validation
    /// revisions. Unknown effects invalidate the proof without discarding the
    /// ordinary net diff.
    rendered_diffs: BTreeMap<(String, TrackedPath), String>,
    unavailable_paths: BTreeSet<TrackedPath>,
    /// Observed workspace changes that no command declared and no tracked path
    /// explains: another writer's, or an undeclared command effect. They are
    /// reported, never charged to this turn's change set.
    unattributed_paths: BTreeSet<TrackedPath>,
    display_keys: HashMap<TrackedPath, String>,
    endpoint_by_spelling: HashMap<TrackedPath, TrackedPath>,
    executor_paths: HashMap<TrackedPath, codex_utils_path_uri::PathUri>,
    executor_roots: HashMap<String, codex_utils_path_uri::PathUri>,
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
            invalidation_causes: Vec::new(),
            owned_operations: VecDeque::new(),
            next_owned_operation: 0,
            owned_operation_bytes: 0,
            owned_operations_complete: true,
            display_roots_by_environment: HashMap::new(),
            remote_environments: HashSet::new(),
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            origin_by_current_path: HashMap::new(),
            mutation_revision: 0,
            attributed_mutation_revision: 0,
            rendered_diffs: BTreeMap::new(),
            unavailable_paths: BTreeSet::new(),
            unattributed_paths: BTreeSet::new(),
            display_keys: HashMap::new(),
            endpoint_by_spelling: HashMap::new(),
            executor_paths: HashMap::new(),
            executor_roots: HashMap::new(),
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
        if self.update_environment_display_roots(display_roots) {
            self.rerender_display_scope();
        }
    }

    fn update_environment_display_roots(
        &mut self,
        display_roots: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> bool {
        let roots = display_roots
            .into_iter()
            .map(|(environment_id, root)| (environment_id, normalize_tracked_path(&root)))
            .collect();
        if self.display_roots_by_environment != roots {
            self.display_roots_by_environment = roots;
            true
        } else {
            false
        }
    }

    fn rerender_display_scope(&mut self) {
        self.rendered_diffs.clear();
        self.display_keys.clear();
        let paths = self
            .baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .cloned()
            .collect();
        self.refresh_unified_diff(paths);
    }

    pub(crate) fn observe_environment_scope(
        &mut self,
        environments: &crate::environment_selection::TurnEnvironmentSnapshot,
        selected: &crate::session::turn_context::TurnEnvironment,
    ) {
        let mut roots = self.display_roots_by_environment.clone();
        for selection in environments.to_selections() {
            self.executor_roots.insert(selection.environment_id.clone(), selection.cwd.clone());
            roots
                .entry(selection.environment_id)
                .or_insert_with(|| PathBuf::from(selection.cwd.inferred_native_path_string()));
        }
        let remote_changed = selected.environment.is_remote()
            && self
                .remote_environments
                .insert(selected.environment_id.clone());
        let roots_changed = self.update_environment_display_roots(roots);
        if roots_changed || remote_changed {
            self.rerender_display_scope();
        }
    }

    pub fn track_delta(&mut self, environment_id: &str, delta: &AppliedPatchDelta) {
        self.track_delta_for_call(environment_id, delta, None);
    }

    pub(crate) fn track_delta_for_call(
        &mut self,
        environment_id: &str,
        delta: &AppliedPatchDelta,
        call_id: Option<&str>,
    ) {
        self.retain_owned_operation(environment_id, delta, call_id);
        if !delta.is_empty() {
            self.record_mutation();
        } else if !delta.is_exact() {
            self.mutation_revision = self.mutation_revision.saturating_add(1);
        }

        if !self.valid {
            // The exact diff is gone, but changed-path consumers (completion
            // attribution, live-session publication) still need this edit.
            self.record_unavailable_delta_paths(environment_id, delta);
            return;
        }

        if !delta.is_exact() {
            self.record_invalidation_cause(serde_json::json!({
                "tool": "apply_patch", "mutation_class": "inexact_patch_delta",
                "environment_id": environment_id, "call_id": call_id,
            }));
            self.record_unavailable_delta_paths(environment_id, delta);
            self.invalidate();
            return;
        }

        if !self.remote_environments.contains(environment_id) {
            self.set_patch_modes(environment_id, delta.preimage_modes().clone());
        }

        let mut changed = HashSet::new();
        for original in delta.changes() {
            let change = delta.endpoint_change(original);
            let spelling = TrackedPath::new(environment_id, &original.path);
            let endpoint = TrackedPath::new(environment_id, &change.path);
            // Check against previously observed endpoints before registering
            // this operation, including known absence after add/delete cycles.
            let continuous = self.patch_preimages_match(environment_id, &change);
            self.executor_paths.insert(endpoint.clone(), change.path_uri().clone());
            if let Some(destination) = change.move_path_uri() {
                self.executor_paths.insert(TrackedPath::new(environment_id, &destination.to_path_buf()), destination.clone());
            }
            if let Some(previous) = self.endpoint_by_spelling.insert(spelling.clone(), endpoint.clone())
                && previous != endpoint
            {
                self.unavailable_paths.insert(previous.clone());
                self.unavailable_paths.insert(endpoint.clone());
                self.unattributed_paths.extend([previous, endpoint.clone()]);
            }
            if let Some(mode) = self.patch_modes.remove(&spelling) {
                self.patch_modes.insert(endpoint, mode);
            }
            let change = &change;
            let mut paths = vec![TrackedPath::new(environment_id, &change.path)];
            if let AppliedPatchFileChange::Update {
                move_path: Some(path),
                ..
            } = &change.change
            {
                paths.push(TrackedPath::new(environment_id, path));
            }
            // An earlier recorded effect that made these paths unavailable left
            // stale expected content and recorded its own attribution (declared
            // command paths stay owned). Do not relabel them as another writer's.
            let already_unavailable = paths
                .iter()
                .all(|path| self.unavailable_paths.contains(path));
            let mut partners = HashSet::new();
            for path in &paths {
                self.collect_diff_partners(path, &mut changed);
                self.collect_diff_partners(path, &mut partners);
            }
            if !continuous
                || partners
                    .iter()
                    .any(|path| self.unavailable_paths.contains(path))
            {
                self.unavailable_paths.extend(partners);
                if !continuous && !already_unavailable {
                    self.unattributed_paths.extend(paths.iter().cloned());
                    self.record_invalidation_cause(serde_json::json!({
                        "tool": "apply_patch", "mutation_class": "patch_preimage_discontinuity",
                        "environment_id": environment_id, "call_id": call_id,
                        "paths": paths.iter().map(|path| self.display_path(path)).collect::<Vec<_>>(),
                        "preimages": self.patch_preimage_fingerprints(environment_id, change),
                    }));
                }
            }
            self.apply_change(environment_id, change);
            for path in &paths {
                self.collect_diff_partners(path, &mut changed);
            }
        }
        self.refresh_unified_diff(changed);
        self.patch_modes.clear();
    }

    /// Keeps the known endpoints of a patch that cannot join the exact diff,
    /// so `changed_paths` still names them. Nothing is rendered.
    fn record_unavailable_delta_paths(&mut self, environment_id: &str, delta: &AppliedPatchDelta) {
        for original in delta.changes() {
            let change = delta.endpoint_change(original);
            self.unavailable_paths
                .insert(TrackedPath::new(environment_id, &change.path));
            if let AppliedPatchFileChange::Update {
                move_path: Some(path),
                ..
            } = &change.change
            {
                self.unavailable_paths
                    .insert(TrackedPath::new(environment_id, path));
            }
        }
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
        if self.first_invalidation_cause.is_none() {
            self.record_invalidation_cause(serde_json::json!({
                "mutation_class": "unknown_mutation",
            }));
        }
        if self.valid {
            // A reported partial diff becoming unavailable is a new state even
            // without a new cause (for example a hook or finalizer mutation).
            self.invalidation_reported = false;
        }
        self.valid = false;
        self.rendered_diffs.clear();
        self.aggregate_dirty = false;
        self.unified_diff = None;
    }

    /// Commands invalidate cached observations, not the apply-patch-only diff.
    pub(crate) fn record_command_execution(&mut self) {
        self.mutation_revision = self.mutation_revision.saturating_add(1);
    }

    pub(crate) fn record_unknown_mutation(&mut self) {
        self.record_mutation();
        self.invalidate();
    }

    pub(crate) fn record_invalidation_cause(&mut self, cause: serde_json::Value) {
        self.first_invalidation_cause.get_or_insert_with(|| cause.clone());
        if !self.invalidation_causes.contains(&cause) {
            // Keep the first cause and a bounded rolling tail. New scopes can
            // warn again; rerecording the same cause cannot create UI spam.
            if self.invalidation_causes.len() == 16 {
                self.invalidation_causes.remove(1);
            }
            self.invalidation_causes.push(cause);
            self.invalidation_reported = false;
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
    /// (`None` once deleted), following a rename to its destination. Uncertain
    /// pairs must not be used to attribute existing-test edits to this turn.
    pub(crate) fn changed_existing_contents(&self) -> Vec<(String, String, Option<String>)> {
        if !self.valid {
            return Vec::new();
        }
        self.baseline_by_path
            .iter()
            .filter_map(|(path, before)| {
                if self.unavailable_paths.contains(path)
                    || self
                        .rename_destination(path)
                        .is_some_and(|destination| self.unavailable_paths.contains(destination))
                {
                    return None;
                }
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

    pub(crate) fn uncertain_content_paths(&self) -> Vec<String> {
        self.unavailable_paths
            .iter()
            .chain(self.baseline_by_path.keys().filter(|_| !self.valid))
            .chain(self.current_by_path.keys().filter(|_| !self.valid))
            .map(|path| self.display_path(path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn current_mutation_revision(&self) -> u64 {
        self.mutation_revision
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

    fn retain_owned_operation(
        &mut self,
        environment_id: &str,
        delta: &AppliedPatchDelta,
        call_id: Option<&str>,
    ) {
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        const MAX_OPERATIONS: usize = 64;
        if !delta.is_exact() || delta.is_empty() {
            return;
        }
        self.next_owned_operation = self.next_owned_operation.saturating_add(1);
        let bytes = delta
            .changes()
            .iter()
            .map(|change| {
                let contents = match &change.change {
                    AppliedPatchFileChange::Add {
                        content,
                        overwritten_content,
                    } => content
                        .len()
                        .saturating_add(overwritten_content.as_ref().map_or(0, String::len)),
                    AppliedPatchFileChange::Delete { content } => content.len(),
                    AppliedPatchFileChange::Update {
                        old_content,
                        new_content,
                        overwritten_move_content,
                        ..
                    } => old_content
                        .len()
                        .saturating_add(new_content.len())
                        .saturating_add(overwritten_move_content.as_ref().map_or(0, String::len)),
                };
                contents
                    .saturating_add(change.path.as_os_str().len())
                    .saturating_add(256)
            })
            .fold(0usize, usize::saturating_add)
            .saturating_add(delta.retained_endpoint_bytes());
        if delta.changes().len() > 256 || bytes > MAX_BYTES {
            self.owned_operations_complete = false;
            return;
        }
        while self.owned_operations.len() >= MAX_OPERATIONS
            || self.owned_operation_bytes.saturating_add(bytes) > MAX_BYTES
        {
            let Some(oldest) = self.owned_operations.pop_front() else {
                break;
            };
            self.owned_operation_bytes -= oldest.retained_bytes;
            self.owned_operations_complete = false;
        }
        self.owned_operation_bytes += bytes;
        self.owned_operations.push_back(OwnedPatchOperation {
            sequence: self.next_owned_operation,
            environment_id: environment_id.into(),
            call_id: call_id.map(str::to_owned),
            delta: delta.clone(),
            retained_bytes: bytes,
        });
    }

    pub(crate) fn owned_operation_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "available_operations": self.owned_operations.iter().map(|operation| operation.sequence).collect::<Vec<_>>(),
            "exact_operation_history_complete": self.owned_operations_complete,
            "retained_bytes": self.owned_operation_bytes,
            "integrated_net_diff": false,
        })
    }

    /// A committed operation is historical evidence, never a repaired net diff
    /// or a claim that a fresh read can reconstruct another writer's ownership.
    pub(crate) fn owned_operation_receipt(&self, sequence: u64) -> Option<serde_json::Value> {
        let operation = self
            .owned_operations
            .iter()
            .find(|operation| operation.sequence == sequence)?;
        let changes = operation
            .delta
            .changes()
            .iter()
            .map(|applied| {
                let (before, after, move_path, displaced_destination) = match &applied.change {
                    AppliedPatchFileChange::Add {
                        content,
                        overwritten_content,
                    } => (
                        overwritten_content.as_deref(),
                        Some(content.as_str()),
                        None,
                        None,
                    ),
                    AppliedPatchFileChange::Delete { content } => {
                        (Some(content.as_str()), None, None, None)
                    }
                    AppliedPatchFileChange::Update {
                        old_content,
                        new_content,
                        move_path,
                        overwritten_move_content,
                    } => (
                        Some(old_content.as_str()),
                        Some(new_content.as_str()),
                        move_path.as_ref(),
                        overwritten_move_content.as_deref(),
                    ),
                };
                serde_json::json!({ "path": applied.path, "before": before, "after": after,
                "move_path": move_path, "displaced_destination": displaced_destination })
            })
            .collect::<Vec<_>>();
        Some(serde_json::json!({
            "version": 1, "scope": "historical_committed_operation", "sequence": sequence,
            "environment_id": operation.environment_id, "call_id": operation.call_id,
            "changes_exact_at_commit": true, "integrated_net_diff": false,
            "current_source_verified": false, "coherent_snapshot_verified": false,
            "changes": changes,
        }))
    }

    /// Returns the net diff of this turn's tracked edits when it is exact, or
    /// the display paths whose changes it cannot diff. An empty list means
    /// command effects invalidated the whole diff. Does not affect publication.
    pub(crate) fn exact_unified_diff(&self) -> Result<Option<String>, Vec<String>> {
        self.ensure_exact_diff()?;
        let diff = if self.aggregate_dirty {
            self.flatten_diff()
        } else {
            self.unified_diff.clone()
        };
        Ok(diff.filter(|diff| !diff.is_empty()))
    }

    fn ensure_exact_diff(&self) -> Result<(), Vec<String>> {
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
        // Creation/deletion needs a mode to be an exact patch. Unknown remote
        // metadata is an export limitation, never permission to query a host namesake.
        let unknown_modes = self
            .baseline_by_path
            .iter()
            .chain(self.current_by_path.iter())
            .filter(|(path, content)| {
                content.mode.is_none()
                    && (self.baseline_by_path.contains_key(*path)
                        != self.current_by_path.contains_key(*path))
            })
            .map(|(path, _)| self.display_path(path))
            .collect::<BTreeSet<_>>();
        if !unknown_modes.is_empty() {
            return Err(unknown_modes.into_iter().collect());
        }
        Ok(())
    }

    /// Machine replay is one executor-root-relative patch, never the UI's
    /// environment-prefixed aggregate. Selection of a partial environment set
    /// is explicit; omitted selection refuses rather than dropping edits.
    pub(crate) fn exact_export_diff(&self, selected: Option<&str>)
        -> Result<Option<(String, String)>, Vec<String>>
    {
        self.ensure_exact_diff()?;
        let environments = self.rendered_diffs.keys().map(|(_, path)| path.environment_id.clone())
            .collect::<BTreeSet<_>>();
        let environment = match selected {
            Some(id) => id.to_string(),
            None if environments.len() > 1 => return Err(environments.into_iter()
                .map(|id| format!("environment {id}: select environment_id and save each environment separately")).collect()),
            None => match environments.into_iter().next() { Some(id) => id, None => return Ok(None) },
        };
        let mut diff = String::new();
        for (_, left) in self.rendered_diffs.keys().filter(|(_, path)| path.environment_id == environment) {
            let right = self.rename_destination(left).unwrap_or(left);
            let left_name = self.relative_executor_path(left).ok_or_else(|| vec![self.display_path(left)])?;
            let right_name = self.relative_executor_path(right).ok_or_else(|| vec![self.display_path(right)])?;
            if let Some(fragment) = self.render_diff_named(left, self.baseline_by_path.get(left),
                right, self.current_by_path.get(right), &left_name, &right_name) {
                diff.push_str(&fragment);
            }
        }
        Ok((!diff.is_empty()).then_some((environment, diff)))
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
            "The turn diff is {status} because command effects or workspace changes could not be tracked exactly. First cause: {}. Recent causes (bounded): {}. Do not claim that no files changed without fresh workspace verification.",
            self.first_invalidation_cause
                .as_ref()
                .unwrap_or(&serde_json::Value::Null),
            serde_json::Value::Array(self.invalidation_causes.clone()),
        ))
    }

    fn record_mutation(&mut self) {
        self.mutation_revision = self.mutation_revision.saturating_add(1);
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
                "# {}: changed, diff unavailable (untracked effects)",
                self.display_path(path)
            );
        }
        Some(aggregate)
    }

    fn refresh_unified_diff(&mut self, changed: HashSet<TrackedPath>) {
        // An invalidated diff stays withdrawn; a display-scope rerender must
        // not rebuild and republish it from stale tracked content.
        if changed.is_empty() || !self.valid {
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

    fn patch_preimage_fingerprints(
        &self,
        environment_id: &str,
        change: &AppliedPatchChange,
    ) -> serde_json::Value {
        use sha2::Digest;
        let describe = |path: &Path, incoming: Option<&str>| {
            let tracked = TrackedPath::new(environment_id, path);
            let expected = self
                .current_by_path
                .get(&tracked)
                .map(|value| value.content.as_str());
            let hash = |value: Option<&str>| {
                value.map(|text| hex::encode(sha2::Sha256::digest(text.as_bytes())))
            };
            serde_json::json!({
                "path": self.display_path(&tracked),
                "expected_sha256": hash(expected), "observed_sha256": hash(incoming),
            })
        };
        match &change.change {
            AppliedPatchFileChange::Add {
                overwritten_content,
                ..
            } => serde_json::json!([describe(&change.path, overwritten_content.as_deref())]),
            AppliedPatchFileChange::Delete { content } => {
                serde_json::json!([describe(&change.path, Some(content))])
            }
            AppliedPatchFileChange::Update {
                old_content,
                move_path,
                overwritten_move_content,
                ..
            } => {
                let mut paths = vec![describe(&change.path, Some(old_content))];
                if let Some(destination) = move_path {
                    paths.push(describe(destination, overwritten_move_content.as_deref()));
                }
                serde_json::Value::Array(paths)
            }
        }
    }

    fn patch_preimages_match(&self, environment_id: &str, change: &AppliedPatchChange) -> bool {
        let matches = |path: &Path, incoming: Option<&str>| {
            let path = TrackedPath::new(environment_id, path);
            // Endpoint metadata outlives a net-zero owned add/delete cycle.
            let known = self.executor_paths.contains_key(&path)
                || self.baseline_by_path.contains_key(&path)
                || self.current_by_path.contains_key(&path);
            !known
                || self
                    .current_by_path
                    .get(&path)
                    .map(|value| value.content.as_str())
                    == incoming
        };
        match &change.change {
            AppliedPatchFileChange::Add {
                overwritten_content,
                ..
            } => matches(&change.path, overwritten_content.as_deref()),
            AppliedPatchFileChange::Delete { content } => matches(&change.path, Some(content)),
            AppliedPatchFileChange::Update {
                old_content,
                move_path,
                overwritten_move_content,
                ..
            } => {
                matches(&change.path, Some(old_content))
                    && move_path
                        .as_ref()
                        .is_none_or(|path| matches(path, overwritten_move_content.as_deref()))
            }
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
        self.render_diff_named(left_path, left_content, right_path, right_content,
            &self.display_path(left_path), &self.display_path(right_path))
    }

    fn render_diff_named(
        &self,
        left_path: &TrackedPath,
        left_content: Option<&TrackedContent>,
        right_path: &TrackedPath,
        right_content: Option<&TrackedContent>,
        left_display: &str,
        right_display: &str,
    ) -> Option<String> {
        let left_text = left_content.map(|content| content.content.as_str());
        let right_text = right_content.map(|content| content.content.as_str());
        if left_text == right_text && (left_path == right_path || left_text.is_none()) {
            return None;
        }

        #[cfg(test)]
        self.rendered_diff_count
            .set(self.rendered_diff_count.get() + 1);

        let left_header = git_quote_path(&format!("a/{left_display}"));
        let right_header = git_quote_path(&format!("b/{right_display}"));
        if left_text == right_text {
            // A content-preserving move still changes the workspace. Include
            // Git's rename metadata so exact exports can replay it.
            return Some(format!(
                "diff --git {left_header} {right_header}\nsimilarity index 100%\nrename from {}\nrename to {}\n",
                git_quote_path(left_display), git_quote_path(right_display)
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
        let mut diff = format!("diff --git {left_header} {right_header}\n");
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
            left_header
        } else {
            DEV_NULL.to_string()
        };
        let new_header = if right_text.is_some() {
            right_header
        } else {
            DEV_NULL.to_string()
        };

        let mut config = similar::TextDiff::configure();
        config.timeout(DIFF_TIMEOUT);
        // Git records are LF-framed. Bare CR is payload, not a line separator.
        let old_lines = left_text.unwrap_or("").split_inclusive('\n').collect::<Vec<_>>();
        let new_lines = right_text.unwrap_or("").split_inclusive('\n').collect::<Vec<_>>();
        let changes = config.diff_slices(&old_lines, &new_lines);
        let _ = writeln!(diff, "--- {old_header}\n+++ {new_header}");
        for group in changes.grouped_ops(3) {
            let first = group.first()?;
            let last = group.last()?;
            let old = first.old_range().start..last.old_range().end;
            let new = first.new_range().start..last.new_range().end;
            let _ = writeln!(diff, "@@ -{} +{} @@", git_hunk_range(old), git_hunk_range(new));
            for op in group {
                for change in changes.iter_changes(&op) {
                    diff.push(match change.tag() {
                        similar::ChangeTag::Equal => ' ',
                        similar::ChangeTag::Delete => '-',
                        similar::ChangeTag::Insert => '+',
                    });
                    let line = change.value();
                    diff.push_str(line);
                    if !line.ends_with('\n') {
                        diff.push_str("\n\\ No newline at end of file\n");
                    }
                }
            }
        }
        Some(diff)
    }

    fn file_mode(&self, path: &TrackedPath) -> Option<&str> {
        // A recreated pathname or a later index entry cannot supply the mode
        // of historical content. Only mutation receipts own this metadata.
        self.patch_modes.get(path).map(String::as_str)
    }



    #[cfg(test)]
    fn rendered_diff_count(&self) -> usize {
        self.rendered_diff_count.get()
    }

    fn display_path(&self, path: &TrackedPath) -> String {
        let display = self.relative_executor_path(path)
            .unwrap_or_else(|| path.path.to_string_lossy().into_owned());
        if self.display_roots_by_environment.len() > 1 && !path.environment_id.is_empty() {
            format!("{}/{display}", path.environment_id)
        } else {
            display
        }
    }

    fn relative_executor_path(&self, path: &TrackedPath) -> Option<String> {
        let relative = if let Some(uri) = self.executor_paths.get(path)
            && let Some(root) = self.executor_roots.get(&path.environment_id)
        {
            let windows = uri.infer_path_convention() == Some(codex_utils_path_uri::PathConvention::Windows);
            let native = uri.inferred_native_path_string();
            let root = root.inferred_native_path_string();
            let separator = if windows { '\\' } else { '/' };
            let root = root.trim_end_matches(separator);
            let relative = native.strip_prefix(root)?.strip_prefix(separator)?;
            if windows { relative.replace('\\', "/") } else { relative.to_string() }
        } else {
            if self.remote_environments.contains(&path.environment_id) { return None; }
            let relative = match self.display_roots_by_environment.get(&path.environment_id) {
                Some(root) => path.path.strip_prefix(root).ok()?,
                None if path.path.is_relative() => &path.path,
                None => return None,
            };
            let text = relative.to_str()?;
            if cfg!(windows) { text.replace('\\', "/") } else { text.to_string() }
        };
        // Never save an absolute or parent-traversing machine replay target.
        if relative.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return None;
        }
        Some(relative)
    }
}

fn git_quote_path(path: &str) -> String {
    if !path.bytes().any(|byte| byte < 32 || byte >= 127 || matches!(byte, b'"' | b'\\')) {
        return path.to_string();
    }
    let mut quoted = String::from("\"");
    for byte in path.bytes() {
        match byte {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            b'\t' => quoted.push_str("\\t"),
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            32..=126 => quoted.push(char::from(byte)),
            _ => { let _ = write!(quoted, "\\{byte:03o}"); }
        }
    }
    quoted.push('"');
    quoted
}

fn git_hunk_range(range: std::ops::Range<usize>) -> String {
    match range.len() {
        0 => format!("{},0", range.start),
        1 => (range.start + 1).to_string(),
        len => format!("{},{}", range.start + 1, len),
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
    executor_path: Option<&codex_utils_path_uri::PathUri>,
    expected: Option<&str>,
    fs: Option<&dyn codex_exec_server::ExecutorFileSystem>,
) -> bool {
    let Some(fs) = fs else { return false; };
    let uri = executor_path.cloned().or_else(|| fs.uses_host_native_paths()
        .then(|| codex_utils_path_uri::PathUri::from_host_native_path(path).ok()).flatten());
    let Some(uri) = uri else {
        return false;
    };
    let limit = expected.map_or(0, str::len);
    match fs.get_metadata(&uri, None).await {
        Ok(metadata) if expected.is_some() && metadata.is_file && !metadata.is_symlink => {}
        Err(error) if expected.is_none() && error.kind() == std::io::ErrorKind::NotFound => return true,
        _ => return false,
    }
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

#[cfg(test)]
#[path = "turn_diff_tracker_export_tests.rs"]
mod export_tests;
