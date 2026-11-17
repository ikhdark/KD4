//! Bounded async provenance, not a scheduler or an interpretation of JS source.
//! Private promise IDs retain no promise/value handles. Promise.all/allSettled/
//! race/any over arrays of intrinsic promises or plain values join through the
//! members V8 actually settled; other aggregate shapes and retention loss stay
//! unknown rather than inventing fork edges. Timestamps never create an edge.
use std::collections::{BTreeMap, BTreeSet};
use super::RuntimeState;

const LIMIT: usize = 8192;
const AGGREGATE_MEMBERS: u32 = 1024;
const AGGREGATE_DEPTH: usize = 16;

/// Members observed by the private aggregate wrapper. `None` is a plain value
/// (no dependency); an unsupported member makes the whole aggregate unknown.
struct Aggregate {
    join_all: bool,
    members: Option<Vec<Option<usize>>>,
}

#[derive(Default)]
struct Promise {
    parent: Option<usize>,
    tool: Option<String>,
    helper: Option<String>,
    aggregate: Option<Aggregate>,
    /// Order of this promise's first V8 resolve/reject hook.
    settled: Option<u64>,
    /// Reaction whose job was running when the first resolve hook fired.
    resolved_by: Option<usize>,
}

#[derive(Default)]
pub(super) struct Capture {
    key: Option<v8::Global<v8::Private>>,
    promises: Vec<Promise>,
    current: Option<usize>,
    stack: Vec<Option<usize>>,
    received: u64,
    resolves: u64,
    dropped: bool,
    graph_deps: BTreeMap<String, Vec<String>>,
    graph_calls: BTreeMap<String, Vec<String>>,
    graph_finished: BTreeSet<String>,
    helpers: BTreeMap<String, Option<Vec<String>>>,
    helper_calls: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    helper_graphs: BTreeMap<String, String>,
}

impl Capture {
    pub(super) fn helper(&mut self, id: String, context: Option<&str>) {
        if self.helpers.len() >= 1024 { self.dropped = true; return; }
        let predecessors = context.and_then(|context| self.graph_predecessors(context))
            .or_else(|| self.predecessors());
        if let Some(context) = context.filter(|context| self.graph_deps.contains_key(*context)) {
            self.helper_graphs.insert(id.clone(), context.to_owned());
        }
        self.helpers.insert(id, predecessors);
    }

    fn helper_predecessors(&self, context: &str) -> Option<Vec<String>> {
        let (group, branch) = context.rsplit_once('/')?;
        let root = self.helpers.get(group)?.as_ref()?;
        Some(self.helper_calls.get(group).and_then(|calls| calls.get(branch)).unwrap_or(root).clone())
    }

    fn helper_join(&self, group: &str) -> Option<Vec<String>> {
        let root = self.helpers.get(group)?.as_ref()?;
        let Some(branches) = self.helper_calls.get(group) else { return Some(root.clone()); };
        let result = branches.values().flatten().cloned().collect::<BTreeSet<_>>();
        (result.len() <= 128).then(|| result.into_iter().collect())
    }
    pub(super) fn graph(&mut self, id: &str, definition: &serde_json::Value) {
        for row in definition.as_array().into_iter().flatten() {
            let Some(name) = row["id"].as_str() else { self.dropped = true; continue; };
            if self.graph_deps.len() >= LIMIT { self.dropped = true; break; }
            self.graph_deps.insert(format!("{id}:{name}"), row["deps"].as_array().into_iter().flatten()
                .filter_map(|dep| dep.as_str().map(|dep| format!("{id}:{dep}"))).collect());
        }
    }

    pub(super) fn graph_finished(&mut self, id: String) {
        if self.graph_finished.len() < LIMIT { self.graph_finished.insert(id); }
        else { self.dropped = true; }
    }

    fn graph_predecessors(&self, context: &str) -> Option<Vec<String>> {
        if self.dropped || self.graph_calls.get(context).is_some_and(|calls| !calls.is_empty()) { return None; }
        let deps = self.graph_deps.get(context)?;
        let mut result = Vec::new();
        for dep in deps {
            if !self.graph_finished.contains(dep) { return None; }
            result.extend(self.graph_calls.get(dep).into_iter().flatten().cloned());
        }
        (result.len() <= 128).then_some(result)
    }

    pub(super) fn result_received(&mut self) { self.received = self.received.saturating_add(1); }

    fn predecessors(&self) -> Option<Vec<String>> {
        if self.dropped { return None; }
        // Before any tool result arrives there cannot be a result-consumption
        // dependency on another nested tool. All such calls form root forks.
        if self.received == 0 { return Some(Vec::new()); }
        self.walk(self.current, 0)
    }

    fn walk(&self, start: Option<usize>, depth: usize) -> Option<Vec<String>> {
        let mut next = start;
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if !seen.insert(id) { return None; }
            let row = self.promises.get(id)?;
            if let Some(tool) = &row.tool { return Some(vec![tool.clone()]); }
            if let Some(helper) = &row.helper { return self.helper_join(helper); }
            if let Some(aggregate) = &row.aggregate { return self.aggregate_join(row, aggregate, depth); }
            next = row.parent;
        }
        None
    }

    /// all/allSettled consume every member only when each member settled before
    /// the aggregate. Otherwise (early all rejection, race, any) the aggregate
    /// was settled inside exactly one member's reaction; that member is the
    /// consumed result. A settling context outside the members stays unknown.
    fn aggregate_join(&self, row: &Promise, aggregate: &Aggregate, depth: usize) -> Option<Vec<String>> {
        if depth >= AGGREGATE_DEPTH { return None; }
        let members = aggregate.members.as_ref()?;
        let resolved = row.settled?;
        if members.is_empty() { return Some(Vec::new()); }
        let all_before = members.iter().flatten().all(|member| self.promises.get(*member)
            .and_then(|member| member.settled).is_some_and(|settled| settled < resolved));
        if aggregate.join_all && all_before {
            let mut result = BTreeSet::new();
            for member in members.iter().flatten() {
                result.extend(self.walk(Some(*member), depth + 1)?);
            }
            return (result.len() <= 128).then(|| result.into_iter().collect());
        }
        let mut next = row.resolved_by;
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if !seen.insert(id) { return None; }
            if members.contains(&Some(id)) { return self.walk(Some(id), depth + 1); }
            next = self.promises.get(id)?.parent;
        }
        None
    }

    fn resolved(&mut self, id: Option<usize>) {
        let current = self.current;
        let Some(row) = id.and_then(|id| self.promises.get_mut(id)) else { return; };
        if row.settled.is_some() { return; }
        self.resolves = self.resolves.saturating_add(1);
        row.settled = Some(self.resolves);
        row.resolved_by = current;
    }
}

pub(super) fn install(scope: &mut v8::PinScope<'_, '_>) {
    let key = v8::Private::new(scope, None);
    let key = v8::Global::new(scope, key);
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() { state.causal.key = Some(key); }
}

fn identity(scope: &mut v8::PinScope<'_, '_>, promise: v8::Local<'_, v8::Promise>) -> Option<usize> {
    let key = v8::Local::new(scope, scope.get_slot::<RuntimeState>()?.causal.key.as_ref()?);
    let value = promise.get_private(scope, key)?;
    v8::Local::<v8::Uint32>::try_from(value).ok().map(|value| value.value() as usize)
}

// V8 calls this on the isolate's own thread. It never polls, logs, takes a host
// lock, touches user properties, or retains a JS promise/result. Private numeric
// tags and bounded Rust metadata cannot extend a tool result's JS lifetime.
pub(super) unsafe extern "C" fn promise_hook(
    kind: v8::PromiseHookType, promise: v8::Local<v8::Promise>, parent: v8::Local<v8::Value>,
) {
    v8::callback_scope!(unsafe scope, promise);
    if scope.get_slot::<RuntimeState>().is_none() { return; }
    match kind {
        v8::PromiseHookType::Init => {
            let parent = v8::Local::<v8::Promise>::try_from(parent).ok()
                .and_then(|parent| identity(scope, parent));
            let Some(state) = scope.get_slot_mut::<RuntimeState>() else { return; };
            if state.causal.promises.len() >= LIMIT { state.causal.dropped = true; return; }
            let id = state.causal.promises.len();
            state.causal.promises.push(Promise { parent, ..Promise::default() });
            // End the mutable runtime-state borrow before borrowing the isolate
            // to create a local handle. The key lookup itself is read-only.
            let Some(key) = scope.get_slot::<RuntimeState>()
                .and_then(|state| state.causal.key.as_ref()) else { return; };
            let key = v8::Local::new(scope, key);
            let value = v8::Integer::new_from_unsigned(scope, id as u32);
            if promise.set_private(scope, key, value.into()) != Some(true)
                && let Some(state) = scope.get_slot_mut::<RuntimeState>() { state.causal.dropped = true; }
        }
        v8::PromiseHookType::Before => {
            let id = identity(scope, promise);
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                if state.causal.stack.len() >= LIMIT { state.causal.dropped = true; return; }
                state.causal.stack.push(state.causal.current);
                state.causal.current = id;
            }
        }
        v8::PromiseHookType::After => {
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                state.causal.current = state.causal.stack.pop().flatten();
            }
        }
        v8::PromiseHookType::Resolve => {
            // Fires before the state change, for resolve and reject alike.
            let id = identity(scope, promise);
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() { state.causal.resolved(id); }
        }
    }
}

pub(super) fn tool(
    scope: &mut v8::PinScope<'_, '_>, promise: v8::Local<'_, v8::Promise>, id: &str,
) -> Option<Vec<String>> {
    let index = identity(scope, promise);
    let context = scope.get_continuation_preserved_embedder_data();
    let context = v8::Local::<v8::String>::try_from(context).ok()
        .map(|value| value.to_rust_string_lossy(scope));
    let state = scope.get_slot_mut::<RuntimeState>()?;
    let helper_context = context.as_deref().filter(|context| context.starts_with("helper-"));
    let predecessors = if let Some(context) = helper_context {
        state.causal.helper_predecessors(context)
    } else { context.as_deref().and_then(|context| state.causal.graph_predecessors(context))
        .or_else(|| state.causal.predecessors()) };
    if let Some((group, branch)) = helper_context.and_then(|context| context.rsplit_once('/')) {
        if !state.causal.dropped && state.causal.helpers.contains_key(group) {
            // Owned read/recovery actions dispatch exactly one native operation.
            // The next action in this branch awaits this operation first.
            state.causal.helper_calls.entry(group.to_owned()).or_default()
                .insert(branch.to_owned(), vec![id.to_owned()]);
            if let Some(graph) = state.causal.helper_graphs.get(group).cloned() {
                state.causal.graph_calls.entry(graph).or_default().push(id.to_owned());
            }
        }
    }
    if let Some(context) = context.filter(|context| !state.causal.dropped && state.causal.graph_deps.contains_key(context)) {
        state.causal.graph_calls.entry(context).or_default().push(id.to_owned());
    }
    if let Some(row) = index.and_then(|index| state.causal.promises.get_mut(index)) {
        // The returned native promise identifies exactly this operation. Its
        // own predecessor edges travel with this dispatch, not future receipts.
        row.tool = Some(id.to_owned());
        row.parent = None;
    } else { state.causal.dropped = true; }
    predecessors
}

pub(super) fn bind_helper(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>, group: String) {
    let index = v8::Local::<v8::Promise>::try_from(value).ok().and_then(|promise| identity(scope, promise));
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        if let Some(row) = index.and_then(|index| state.causal.promises.get_mut(index)) {
            row.helper = Some(group);
            row.parent = None;
        } else { state.causal.dropped = true; }
    }
}

/// Membership reported by the private wrapper around the intrinsic Promise
/// statics after the original builtin returned. Reads only V8 type checks,
/// prototypes and own-property presence; never a getter or user property.
/// Members are intrinsic promises without an own `then`, or primitives.
pub(super) fn aggregate(
    scope: &mut v8::PinScope<'_, '_>, join_all: bool,
    result: v8::Local<'_, v8::Value>, members: v8::Local<'_, v8::Value>,
) {
    let Ok(result) = v8::Local::<v8::Promise>::try_from(result) else { return; };
    let Some(index) = identity(scope, result) else { return; };
    let prototype = result.get_prototype(scope);
    let then = v8::String::new(scope, "then");
    let rows = v8::Local::<v8::Array>::try_from(members).ok()
        .filter(|members| members.length() <= AGGREGATE_MEMBERS)
        .zip(prototype).zip(then).and_then(|((members, prototype), then)| {
            let mut rows = Vec::new();
            for position in 0..members.length() {
                let member = members.get_index(scope, position)?;
                if !member.is_object() { rows.push(None); continue; }
                let promise = v8::Local::<v8::Promise>::try_from(member).ok()?;
                let intrinsic = promise.get_prototype(scope)
                    .is_some_and(|value| value.strict_equals(prototype))
                    && promise.has_own_property(scope, then.into()) == Some(false);
                if !intrinsic { return None; }
                rows.push(Some(identity(scope, promise)?));
            }
            Some(rows)
        });
    if let Some(row) = scope.get_slot_mut::<RuntimeState>()
        .and_then(|state| state.causal.promises.get_mut(index)) {
        row.aggregate = Some(Aggregate { join_all, members: rows });
    }
}
