//! Bounded async provenance, not a scheduler or an interpretation of JS source.
//! Private promise IDs and weak settlement probes do not retain JS results.
//! The intrinsic aggregate algorithms run on the original arguments. Only
//! proven member reactions can supply aggregate edges; unsupported shapes
//! and retention loss stay unknown. Timestamps never create an edge.
use std::collections::{BTreeMap, BTreeSet};
use super::RuntimeState;

const LIMIT: usize = 8192;
const AGGREGATE_MEMBERS: u32 = 1024;
const AGGREGATE_DEPTH: usize = 16;
const WALK_STEPS: usize = LIMIT;
const MAX_PREDECESSORS: usize = 128;

#[derive(Clone, Copy)]
enum AggregateKind {
    All,
    AllSettled,
    Race,
    Any,
}

enum Aggregate {
    Unknown,
    Pending {
        kind: AggregateKind,
        // Exact reaction promise IDs bound during builtin registration.
        // A primitive has a reaction but no tool dependency.
        reactions: BTreeMap<usize, Option<usize>>,
        probe: Option<v8::Weak<v8::Promise>>,
    },
    Consumed(Vec<usize>),
}

struct Settlement {
    aggregate: usize,
    reaction: usize,
    probe: v8::Weak<v8::Promise>,
}

struct Reaction {
    previous: Option<usize>,
    settlements: Vec<Settlement>,
}

struct Walk {
    remaining: usize,
    memo: BTreeMap<(usize, usize), Option<Vec<String>>>,
}

impl Walk {
    fn step(&mut self) -> Option<()> {
        self.remaining = self.remaining.checked_sub(1)?;
        Some(())
    }
}

#[derive(Default)]
struct Promise {
    parent: Option<usize>,
    tool: Option<String>,
    helper: Option<String>,
    aggregate: Option<Aggregate>,
}

#[derive(Default)]
pub(super) struct Capture {
    key: Option<v8::Global<v8::Private>>,
    promises: Vec<Promise>,
    current: Option<usize>,
    stack: Vec<Reaction>,
    received: u64,
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
        self.walk(self.current, 0, &mut Walk { remaining: WALK_STEPS, memo: BTreeMap::new() })
    }

    fn walk(&self, start: Option<usize>, depth: usize, walk: &mut Walk) -> Option<Vec<String>> {
        let mut next = start;
        let mut path = Vec::new();
        let result = loop {
            walk.step()?;
            let Some(id) = next else { break None; };
            let key = (id, depth);
            if let Some(result) = walk.memo.get(&key) { break result.clone(); }
            // The same key reached while still visiting it is a cycle.
            walk.memo.insert(key, None);
            path.push(key);
            let Some(row) = self.promises.get(id) else { break None; };
            if let Some(tool) = &row.tool { break Some(vec![tool.clone()]); }
            if let Some(helper) = &row.helper { break self.helper_join(helper); }
            if let Some(aggregate) = &row.aggregate {
                break self.aggregate_join(aggregate, depth, walk);
            }
            next = row.parent;
        };
        for key in path {
            walk.memo.insert(key, result.clone());
        }
        result
    }

    fn aggregate_join(&self, aggregate: &Aggregate, depth: usize, walk: &mut Walk) -> Option<Vec<String>> {
        if depth >= AGGREGATE_DEPTH { return None; }
        let Aggregate::Consumed(members) = aggregate else { return None; };
        let mut result = BTreeSet::new();
        for &member in members {
            walk.step()?;
            result.extend(self.walk(Some(member), depth + 1, walk)?);
            if result.len() > MAX_PREDECESSORS { return None; }
        }
        Some(result.into_iter().collect())
    }

    fn resolving(&mut self, aggregate: Option<usize>) {
        let Some(aggregate) = aggregate else { return; };
        let Some(row) = self.promises.get_mut(aggregate) else { return; };
        let Some(Aggregate::Pending { reactions, probe, .. }) = row.aggregate.as_mut() else { return; };
        let (Some(frame), Some(reaction), Some(probe)) =
            (self.stack.last_mut(), self.current, probe.take())
        else {
            // A second Resolve in the same job can be an abrupt thenable
            // lookup, not the rejection arm of the combinator.
            row.aggregate = Some(Aggregate::Unknown);
            return;
        };
        if reactions.contains_key(&reaction) {
            frame.settlements.push(Settlement { aggregate, reaction, probe });
        } else {
            row.aggregate = Some(Aggregate::Unknown);
        }
    }

    fn settled(&mut self, token: Settlement, state: Option<v8::PromiseState>) {
        use v8::PromiseState::{Fulfilled, Rejected};
        use AggregateKind::{All, AllSettled, Any, Race};
        let Some(row) = self.promises.get_mut(token.aggregate) else { return; };
        let Some(Aggregate::Pending { kind, reactions, .. }) = row.aggregate.as_ref() else { return; };
        row.aggregate = Some(match (kind, state) {
            (All | AllSettled, Some(Fulfilled)) | (Any, Some(Rejected)) =>
                Aggregate::Consumed(reactions.values().flatten().copied().collect()),
            (All, Some(Rejected)) | (Any, Some(Fulfilled)) | (Race, Some(Fulfilled | Rejected)) =>
                Aggregate::Consumed(reactions[&token.reaction].into_iter().collect()),
            // Resolve ran, but the promise was not observably settled in
            // that exact reaction. Do not assign thenable adoption by guess.
            _ => Aggregate::Unknown,
        });
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
                state.causal.stack.push(Reaction { previous: state.causal.current, settlements: Vec::new() });
                state.causal.current = id;
            }
        }
        v8::PromiseHookType::After => {
            if let Some(frame) = scope.get_slot_mut::<RuntimeState>().and_then(|state| {
                let frame = state.causal.stack.pop()?;
                state.causal.current = frame.previous;
                Some(frame)
            }) {
                for probe in frame.settlements {
                    let outcome = probe.probe.to_local(scope).map(|promise| promise.state());
                    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                        state.causal.settled(probe, outcome);
                    }
                }
            }
        }
        v8::PromiseHookType::Resolve => {
            // Fires before the state change, for resolve and reject alike.
            let id = identity(scope, promise);
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() { state.causal.resolving(id); }
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

/// Inspect an own descriptor field without reading the target property. This
/// must only be used on ordinary objects (callers exclude user proxies).
fn descriptor_field<'s>(
    scope: &mut v8::PinScope<'s, '_>, object: v8::Local<'s, v8::Object>,
    key: v8::Local<'s, v8::Name>, field: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let descriptor = v8::Local::<v8::Object>::try_from(object.get_own_property_descriptor(scope, key)?).ok()?;
    let field = v8::String::new(scope, field)?;
    if descriptor.has_own_property(scope, field.into()) != Some(true) { return None; }
    descriptor.get(scope, field.into())
}

fn descriptor_matches<'s>(
    scope: &mut v8::PinScope<'s, '_>, object: v8::Local<'s, v8::Object>,
    key: v8::Local<'s, v8::Name>, field: &str, expected: v8::Local<'s, v8::Value>,
) -> bool {
    descriptor_field(scope, object, key, field).is_some_and(|value| value.strict_equals(expected))
}

/// Bootstrap invoked once from promise_aggregates.js. The proxy traps only
/// calls; properties, descriptors and constructability come from the builtin.
/// Keeping the apply trap native avoids JS wrapper frames and forwards the
/// engine-provided raw receiver (including undefined and primitive receivers).
pub(super) fn aggregate(
    scope: &mut v8::PinScope<'_, '_>, _bootstrap: bool,
    constructor: v8::Local<'_, v8::Value>, prototypes: v8::Local<'_, v8::Value>,
) {
    let _ = install_aggregates(scope, constructor, prototypes);
}

fn install_aggregates<'s>(
    scope: &mut v8::PinScope<'s, '_>, constructor: v8::Local<'_, v8::Value>,
    prototypes: v8::Local<'_, v8::Value>,
) -> Option<()> {
    let constructor = v8::Local::new(scope, v8::Local::<v8::Object>::try_from(constructor).ok()?);
    let prototypes = v8::Local::<v8::Array>::try_from(prototypes).ok()?;
    let array = v8::Local::<v8::Object>::try_from(prototypes.get_index(scope, 0)?).ok()?;
    let array_iterator = v8::Local::<v8::Object>::try_from(prototypes.get_index(scope, 1)?).ok()?;
    let species_key = v8::Local::<v8::Name>::try_from(prototypes.get_index(scope, 2)?).ok()?;
    let prototype_key = v8::String::new(scope, "prototype")?;
    let resolve_key = v8::String::new(scope, "resolve")?;
    let then_key = v8::String::new(scope, "then")?;
    let next_key = v8::String::new(scope, "next")?;
    let iterator_key = v8::Symbol::get_iterator(scope);
    let prototype = v8::Local::<v8::Object>::try_from(
        descriptor_field(scope, constructor, prototype_key.into(), "value")?
    ).ok()?;
    let resolve = descriptor_field(scope, constructor, resolve_key.into(), "value")?;
    let then = descriptor_field(scope, prototype, then_key.into(), "value")?;
    let iterator = descriptor_field(scope, array, iterator_key.into(), "value")?;
    let next = descriptor_field(scope, array_iterator, next_key.into(), "value")?;
    let species = descriptor_field(scope, constructor, species_key, "get")?;
    let intrinsics = v8::Array::new_with_elements(scope, &[
        constructor.into(), prototype.into(), resolve, then, array.into(),
        array_iterator.into(), iterator, next, species_key.into(), species,
    ]);
    let apply = v8::String::new(scope, "apply")?;
    for (kind, name) in ["all", "allSettled", "race", "any"].into_iter().enumerate() {
        let key = v8::String::new(scope, name)?;
        let original = v8::Local::<v8::Object>::try_from(
            descriptor_field(scope, constructor, key.into(), "value")?
        ).ok()?;
        let mode = v8::Integer::new_from_unsigned(scope, kind as u32);
        let data = v8::Array::new_with_elements(scope, &[mode.into(), intrinsics.into()]);
        let trap = v8::Function::builder(apply_aggregate).data(data.into()).build(scope)?;
        let handler = v8::Object::new(scope);
        let null = v8::null(scope);
        if handler.set_prototype(scope, null.into()) != Some(true)
            || handler.create_data_property(scope, apply.into(), trap.into()) != Some(true)
        { return None; }
        let proxy = v8::Proxy::new(scope, original, handler)?;
        if constructor.define_own_property(scope, key.into(), proxy.into(), v8::PropertyAttribute::DONT_ENUM)
            != Some(true) { return None; }
    }
    Some(())
}

fn aggregate_members<'s>(
    scope: &mut v8::PinScope<'s, '_>, intrinsics: v8::Local<'s, v8::Array>,
    input: v8::Local<'s, v8::Value>,
) -> Option<Vec<Option<usize>>> {
    if input.is_proxy() { return None; }
    let input = v8::Local::<v8::Array>::try_from(input).ok()?;
    if input.length() > AGGREGATE_MEMBERS { return None; }
    let constructor = v8::Local::<v8::Object>::try_from(intrinsics.get_index(scope, 0)?).ok()?;
    let prototype = v8::Local::<v8::Object>::try_from(intrinsics.get_index(scope, 1)?).ok()?;
    let expected_resolve = intrinsics.get_index(scope, 2)?;
    let expected_then = intrinsics.get_index(scope, 3)?;
    let array = v8::Local::<v8::Object>::try_from(intrinsics.get_index(scope, 4)?).ok()?;
    let array_iterator = v8::Local::<v8::Object>::try_from(intrinsics.get_index(scope, 5)?).ok()?;
    let expected_iterator = intrinsics.get_index(scope, 6)?;
    let expected_next = intrinsics.get_index(scope, 7)?;
    let species_key = v8::Local::<v8::Name>::try_from(intrinsics.get_index(scope, 8)?).ok()?;
    let expected_species = intrinsics.get_index(scope, 9)?;
    let resolve = v8::String::new(scope, "resolve")?;
    let then = v8::String::new(scope, "then")?;
    let ctor = v8::String::new(scope, "constructor")?;
    let iterator = v8::Symbol::get_iterator(scope);
    let next = v8::String::new(scope, "next")?;
    if !input.get_prototype(scope)?.strict_equals(array.into())
        || input.has_own_property(scope, iterator.into()) != Some(false)
        || !descriptor_matches(scope, array, iterator.into(), "value", expected_iterator)
        || !descriptor_matches(scope, array_iterator, next.into(), "value", expected_next)
        || !descriptor_matches(scope, constructor, resolve.into(), "value", expected_resolve)
        || !descriptor_matches(scope, constructor, species_key, "get", expected_species)
        || !descriptor_matches(scope, prototype, then.into(), "value", expected_then)
        || !descriptor_matches(scope, prototype, ctor.into(), "value", constructor.into())
    { return None; }

    let mut members = Vec::with_capacity(input.length() as usize);
    for index in 0..input.length() {
        let key = v8::Integer::new_from_unsigned(scope, index).to_string(scope)?;
        // No array hole, inherited index, or accessor is read here. The actual
        // builtin still receives the original iterable on the unknown path.
        let member = descriptor_field(scope, input.into(), key.into(), "value")?;
        if !member.is_object() { members.push(None); continue; }
        let promise = v8::Local::<v8::Promise>::try_from(member).ok()?;
        if !promise.get_prototype(scope)?.strict_equals(prototype.into())
            || promise.has_own_property(scope, then.into()) != Some(false)
            || promise.has_own_property(scope, ctor.into()) != Some(false)
        { return None; }
        members.push(Some(identity(scope, promise)?));
    }
    Some(members)
}

fn bound_reactions(
    capture: &Capture, first: usize, members: Vec<Option<usize>>,
) -> Option<BTreeMap<usize, Option<usize>>> {
    if capture.dropped { return None; }
    // Intrinsic then makes exactly one child promise per actual member. Its
    // Init hook reports the reaction parent, including primitive normalization.
    let mut children = capture.promises.get(first..)?.iter().enumerate()
        .filter_map(|(offset, row)| row.parent.map(|parent| (first + offset, parent)));
    let mut reactions = BTreeMap::new();
    for member in members {
        let (child, parent) = children.next()?;
        if member.map_or_else(|| !(first..child).contains(&parent), |id| id != parent) {
            return None;
        }
        reactions.insert(child, member);
    }
    children.next().is_none().then_some(reactions)
}

fn apply_aggregate(
    scope: &mut v8::PinScope<'_, '_>, args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Ok(original) = v8::Local::<v8::Function>::try_from(args.get(0)) else { return; };
    let this = args.get(1);
    // Proxy apply supplies this private dense arguments array, not the iterable.
    let Ok(argv) = v8::Local::<v8::Array>::try_from(args.get(2)) else { return; };
    let argument = if argv.length() == 0 { None } else { argv.get_index(scope, 0) };
    let data = v8::Local::<v8::Array>::try_from(args.data()).ok();
    let mode = data.and_then(|data| data.get_index(scope, 0))
        .and_then(|mode| v8::Local::<v8::Uint32>::try_from(mode).ok());
    let kind = mode.and_then(|mode| match mode.value() {
        0 => Some(AggregateKind::All),
        1 => Some(AggregateKind::AllSettled),
        2 => Some(AggregateKind::Race),
        3 => Some(AggregateKind::Any),
        _ => None,
    });
    let intrinsics = data.and_then(|data| data.get_index(scope, 1))
        .and_then(|value| v8::Local::<v8::Array>::try_from(value).ok());
    let intrinsic_receiver = intrinsics.and_then(|data| data.get_index(scope, 0))
        .is_some_and(|constructor| this.strict_equals(constructor));
    let captured = if intrinsic_receiver {
        intrinsics.zip(argument)
            .and_then(|(intrinsics, input)| aggregate_members(scope, intrinsics, input))
            .zip(scope.get_slot::<RuntimeState>().map(|state| state.causal.promises.len()))
    } else { None };

    // The statics have one input parameter. Any surplus arguments are ignored
    // by the original builtin regardless of receiver.
    let Some(result) = original.call(scope, this, argument.as_slice()) else { return; };
    retval.set(result);

    if !intrinsic_receiver { return; }
    let Ok(promise) = v8::Local::<v8::Promise>::try_from(result) else { return; };
    let Some(id) = identity(scope, promise) else { return; };
    let reactions = captured.and_then(|(members, first)| {
        if id < first { return None; }
        bound_reactions(&scope.get_slot::<RuntimeState>()?.causal, first, members)
    });
    let aggregate = match (kind, reactions, promise.state()) {
        (Some(_), Some(reactions), v8::PromiseState::Fulfilled | v8::PromiseState::Rejected)
            if reactions.is_empty() => Aggregate::Consumed(Vec::new()),
        (Some(kind), Some(reactions), v8::PromiseState::Pending) if !reactions.is_empty() =>
            Aggregate::Pending { kind, reactions, probe: Some(v8::Weak::new(scope, promise)) },
        _ => Aggregate::Unknown,
    };
    if let Some(row) = scope.get_slot_mut::<RuntimeState>()
        .and_then(|state| state.causal.promises.get_mut(id)) {
        row.aggregate = Some(aggregate);
    }
}
