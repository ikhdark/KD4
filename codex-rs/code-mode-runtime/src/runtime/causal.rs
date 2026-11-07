//! Bounded async provenance, not a scheduler or an interpretation of JS source.
//! Private promise IDs retain no promise/value handles. Unsupported aggregate
//! adoption and retention loss stay unknown rather than inventing fork edges.
use std::collections::{BTreeMap, BTreeSet};
use super::RuntimeState;

const LIMIT: usize = 8192;

#[derive(Default)]
struct Promise {
    parent: Option<usize>,
    tool: Option<String>,
}

#[derive(Default)]
pub(super) struct Capture {
    key: Option<v8::Global<v8::Private>>,
    promises: Vec<Promise>,
    current: Option<usize>,
    stack: Vec<Option<usize>>,
    received: u64,
    dropped: bool,
    graph_deps: BTreeMap<String, Vec<String>>,
    graph_calls: BTreeMap<String, Vec<String>>,
    graph_finished: BTreeSet<String>,
}

impl Capture {
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
        let mut next = self.current;
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if !seen.insert(id) { return None; }
            let row = self.promises.get(id)?;
            if let Some(tool) = &row.tool { return Some(vec![tool.clone()]); }
            next = row.parent;
        }
        None
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
            state.causal.promises.push(Promise { parent, tool: None });
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
        v8::PromiseHookType::Resolve => {}
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
    let predecessors = context.as_deref().and_then(|context| state.causal.graph_predecessors(context))
        .or_else(|| state.causal.predecessors());
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
