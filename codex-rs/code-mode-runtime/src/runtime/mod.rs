mod callbacks;
mod globals;
mod module_loader;
mod output_projection;
mod timers;
mod value;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::host::MAX_FRAME_BYTES;
use codex_code_mode_protocol::normalize_code_mode_identifier;
use codex_protocol::ToolName;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

const RUNTIME_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Frame room for everything a runtime response carries besides its content
/// items: envelope, cell ID, bounded error text (escaping can grow it
/// sixfold), and output-loss counters.
const RUNTIME_RESPONSE_FRAME_RESERVE: usize = 1024 * 1024;
/// Content one cell may buffer, measured as its JSON encoding, so any
/// response assembled from admitted output fits one host IPC frame.
pub(crate) const MAX_BUFFERED_OUTPUT_BYTES: usize =
    MAX_FRAME_BYTES - RUNTIME_RESPONSE_FRAME_RESERVE;
const _: () = assert!(MAX_BUFFERED_OUTPUT_BYTES == 63 * 1024 * 1024);
pub(crate) const MAX_ERROR_TEXT_BYTES: usize = 16 * 1024;
const ERROR_TRUNCATION_SUFFIX: &str = "\n[code mode error truncated]";
const OUTPUT_LIMIT_MESSAGE: &str = "Code mode output was truncated because the cell's buffered output exceeded its 63 MiB encoded limit.";
/// Encoded size of an empty text item plus its array separator: the least any
/// text output adds to the buffered budget beyond its raw bytes.
const TEXT_ITEM_ENCODING_OVERHEAD: usize = r#"{"type":"input_text","text":""}"#.len() + 1;

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse {
        id: String,
        result: JsonValue,
    },
    ToolError {
        id: String,
        error_text: String,
    },
    ToolResponseAfterDeadline {
        id: String,
        result: JsonValue,
        error_text: String,
    },
    NotificationResponse {
        id: String,
    },
    NotificationError {
        id: String,
        error_text: String,
    },
    TimersReady,
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    ContentItem {
        item: FunctionCallOutputContentItem,
        admitted_bytes: usize,
    },
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
        timeout_ms: u64,
    },
    Notify {
        id: Option<String>,
        call_id: String,
        text: String,
    },
    Result {
        stored_value_writes: HashMap<String, StoredValue>,
        error_text: Option<String>,
        output_loss: Option<codex_code_mode_protocol::OutputLoss>,
    },
    ThreadPanicked,
}

/// A session value and the serialized size of its entry, measured once when
/// stored so later cells and commits never reserialize it.
#[derive(Clone, Debug)]
pub(crate) struct StoredValue {
    pub(crate) value: Arc<JsonValue>,
    /// Trusted display edit recipes captured from the tool-owned projector.
    /// Plain JSON values never infer or synthesize these registrations.
    pub(crate) presentation: Option<Arc<JsonValue>>,
    /// Immutable wire representation shared by loads and cell snapshots. Each
    /// load still parses a fresh JS value, so callers cannot mutate the store.
    pub(crate) serialized: Option<Arc<serde_json::value::RawValue>>,
    /// Transaction-only tombstone; never installed in the committed values map.
    pub(crate) deleted: bool,
    /// Serialized key, value and presentation bytes charged to the session limit.
    pub(crate) bytes: usize,
    /// Keys read by the producing cell. None means the bounded read set
    /// overflowed and the owner must compare the complete snapshot.
    pub(crate) read_dependencies: Option<Arc<HashSet<String>>>,
}

impl StoredValue {
    pub(crate) fn new(key: &str, value: JsonValue) -> Self {
        let serialized: Option<Arc<serde_json::value::RawValue>> =
            serde_json::value::to_raw_value(&value).ok().map(Arc::from);
        let bytes = serialized
            .as_ref()
            .and_then(|json| {
                stored_value_entry_bytes(key, &JsonValue::Null)
                    .checked_sub(4)
                    .and_then(|key_bytes| key_bytes.checked_add(json.get().len()))
            })
            .unwrap_or(usize::MAX);
        Self {
            bytes,
            value: Arc::new(value),
            presentation: None,
            serialized,
            deleted: false,
            read_dependencies: Some(Arc::new(HashSet::new())),
        }
    }

    pub(crate) fn deletion(key: &str) -> Self {
        let mut value = Self::new(key, JsonValue::Null);
        value.deleted = true;
        value
    }

    pub(crate) fn with_presentation(mut self, presentation: Option<JsonValue>) -> Self {
        if let Some(presentation) = presentation {
            let mut counter = JsonByteCounter::default();
            let bytes = serde_json::to_writer(&mut counter, &presentation)
                .map_or(usize::MAX, |()| counter.bytes);
            self.bytes = self.bytes.saturating_add(bytes);
            self.presentation = Some(Arc::new(presentation));
        }
        self
    }
}

pub(crate) struct OutputAdmission {
    state: Mutex<OutputAdmissionState>,
    max_bytes: usize,
    yield_pending: AtomicBool,
}

struct OutputAdmissionState {
    admitted_bytes: usize,
    overflow_reported: bool,
    output_loss: codex_code_mode_protocol::OutputLoss,
}

impl OutputAdmission {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(OutputAdmissionState {
                admitted_bytes: 0,
                overflow_reported: false,
                output_loss: codex_code_mode_protocol::OutputLoss::default(),
            }),
            max_bytes,
            yield_pending: AtomicBool::new(false),
        }
    }

    pub(super) fn admit_yield(&self) -> Option<RuntimeEvent> {
        // Repeated requests while the actor has not observed the first one have
        // the same effect. Keep synchronous JavaScript loops from flooding IPC.
        (!self.yield_pending.swap(true, Ordering::AcqRel)).then_some(RuntimeEvent::YieldRequested)
    }

    pub(crate) fn release_yield(&self) {
        self.yield_pending.store(false, Ordering::Release);
    }

    fn admit(&self, item: FunctionCallOutputContentItem) -> Option<RuntimeEvent> {
        let admitted_bytes = output_item_bytes(&item);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(next) = state
            .admitted_bytes
            .checked_add(admitted_bytes)
            .filter(|next| *next <= self.max_bytes)
        {
            state.admitted_bytes = next;
            return Some(RuntimeEvent::ContentItem {
                item,
                admitted_bytes,
            });
        }

        Self::record_loss(&mut state, output_payload_bytes(&item))
    }

    fn record_loss(state: &mut OutputAdmissionState, bytes: usize) -> Option<RuntimeEvent> {
        state.output_loss.discarded_items = state.output_loss.discarded_items.saturating_add(1);
        state.output_loss.discarded_bytes_lower_bound = state
            .output_loss
            .discarded_bytes_lower_bound
            .saturating_add(bytes as u64);
        if !state.overflow_reported {
            state.overflow_reported = true;
            return Some(RuntimeEvent::ContentItem {
                item: FunctionCallOutputContentItem::InputText {
                    text: OUTPUT_LIMIT_MESSAGE.to_string(),
                },
                admitted_bytes: 0,
            });
        }
        None
    }

    /// Rejects text whose raw bytes alone cannot fit, before copying it out of
    /// V8. Admission still charges the full encoded size after conversion.
    pub(super) fn reject_before_conversion(&self, bytes: usize) -> Option<Option<RuntimeEvent>> {
        self.reject_before_conversion_with_overhead(bytes, TEXT_ITEM_ENCODING_OVERHEAD)
    }

    pub(super) fn reject_before_conversion_with_overhead(
        &self,
        bytes: usize,
        overhead: usize,
    ) -> Option<Option<RuntimeEvent>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let required = bytes.saturating_add(overhead);
        if required <= self.max_bytes.saturating_sub(state.admitted_bytes) {
            return None;
        }
        Some(Self::record_loss(&mut state, bytes))
    }

    fn output_loss(&self) -> Option<codex_code_mode_protocol::OutputLoss> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.output_loss.discarded_items > 0).then(|| state.output_loss.clone())
    }

    pub(crate) fn release(&self, released_bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.admitted_bytes >= released_bytes);
        state.admitted_bytes = state.admitted_bytes.saturating_sub(released_bytes);
        if state.admitted_bytes == 0 {
            state.overflow_reported = false;
        }
    }
}

/// Bytes an item occupies in a serialized response's content array: its JSON
/// encoding, which matches the host wire item, plus a separator. Escaping is
/// charged, so admitted output fits a frame however much of it is escaped.
fn output_item_bytes(item: &FunctionCallOutputContentItem) -> usize {
    let mut counter = JsonByteCounter::default();
    match serde_json::to_writer(&mut counter, item) {
        Ok(()) => counter.bytes.saturating_add(1),
        Err(_) => usize::MAX,
    }
}

/// Raw payload bytes, a lower bound on what discarding an item loses.
fn output_payload_bytes(item: &FunctionCallOutputContentItem) -> usize {
    match item {
        FunctionCallOutputContentItem::InputText { text } => text.len(),
        FunctionCallOutputContentItem::InputImage { image_url, .. } => image_url.len(),
    }
}

struct RuntimeStart {
    config: RuntimeConfig,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    isolate_handle_tx: tokio::sync::oneshot::Sender<Result<StartupIsolateHandle, String>>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    task_failure_handler: Option<TaskFailureHandler>,
}

fn spare_runtime() -> &'static Mutex<Option<std_mpsc::Sender<RuntimeStart>>> {
    static SPARE: std::sync::OnceLock<Mutex<Option<std_mpsc::Sender<RuntimeStart>>>> =
        std::sync::OnceLock::new();
    SPARE.get_or_init(|| Mutex::new(None))
}

fn create_spare_runtime() -> std::io::Result<std_mpsc::Sender<RuntimeStart>> {
    let (sender, receiver) = std_mpsc::channel::<RuntimeStart>();
    thread::Builder::new()
        .name("code-mode-spare".into())
        .spawn(move || {
            if let Err(error) = ensure_v8_initialized() {
                if let Ok(start) = receiver.recv() {
                    let _ = start.isolate_handle_tx.send(Err(error));
                }
                return;
            }
            // Isolates and contexts never cross threads and never execute a second
            // cell. Only the startup request crosses this one-use channel.
            let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
            isolate.set_host_import_module_dynamically_callback(
                module_loader::dynamic_import_callback,
            );
            isolate.set_promise_reject_callback(module_loader::promise_rejected);
            v8::scope!(let scope, isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let projector = output_projection::prepare(scope);
            let Ok(start) = receiver.recv() else {
                return;
            };
            let event_tx = start.event_tx.clone();
            let failure_handler = start.task_failure_handler.clone();
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                let projector = match projector {
                    Ok(projector) => projector,
                    Err(error) => {
                        let _ = start.isolate_handle_tx.send(Err(error));
                        return;
                    }
                };
                run_runtime(
                    scope,
                    projector,
                    start.config,
                    start.event_tx,
                    start.command_rx,
                    start.isolate_handle_tx,
                    start.runtime_command_tx,
                );
            })) {
                report_runtime_panic(payload, event_tx, failure_handler);
            }
        })?;
    Ok(sender)
}

pub(crate) fn prewarm_runtime() {
    let mut spare = spare_runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if spare.is_none() {
        match create_spare_runtime() {
            Ok(sender) => *spare = Some(sender),
            Err(error) => tracing::warn!(%error, "code mode runtime prewarm failed"),
        }
    }
}

pub(crate) async fn spawn_runtime_with_catalog(
    stored_values: HashMap<String, StoredValue>,
    request: ExecuteRequest,
    enabled_tools: Arc<EnabledToolCatalog>,
    default_tool_timeout_ms: u64,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    output_admission: Arc<OutputAdmission>,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<(std_mpsc::Sender<RuntimeCommand>, RuntimeTerminationHandle), String> {
    let (command_tx, command_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let (isolate_handle_tx, isolate_handle_rx) = tokio::sync::oneshot::channel();
    let ExecuteRequest {
        tool_call_id,
        source,
        ..
    } = request;
    let config = RuntimeConfig {
        tool_call_id,
        enabled_tools,
        source,
        stored_values,
        default_tool_timeout_ms,
        output_admission,
    };

    let sender = spare_runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .map(Ok)
        .unwrap_or_else(create_spare_runtime)
        .map_err(|error| format!("failed to start code mode runtime: {error}"))?;
    let start = RuntimeStart {
        config,
        event_tx,
        command_rx,
        isolate_handle_tx,
        runtime_command_tx,
        task_failure_handler,
    };
    if let Err(error) = sender.send(start) {
        // The spare died before accepting work; the returned request has never
        // run and can safely be delivered to a replacement exactly once.
        create_spare_runtime()
            .map_err(|error| error.to_string())?
            .send(error.0)
            .map_err(|_| "code mode runtime unavailable".to_string())?;
    }
    prewarm_runtime();

    let isolate_handle =
        receive_runtime_startup(isolate_handle_rx, RUNTIME_STARTUP_TIMEOUT).await?;
    Ok((command_tx, isolate_handle.into_inner()))
}

// If startup times out or its caller is cancelled after the runtime sends a handle,
// dropping the unclaimed message must stop user code on that runtime thread.
struct StartupIsolateHandle(Option<RuntimeTerminationHandle>);

pub(crate) struct RuntimeTerminationHandle {
    isolate: v8::IsolateHandle,
    command_tx: std_mpsc::Sender<RuntimeCommand>,
}

impl RuntimeTerminationHandle {
    pub(crate) fn terminate_execution(&self) -> bool {
        let terminated = self.isolate.terminate_execution();
        let _ = self.command_tx.send(RuntimeCommand::Terminate);
        terminated
    }
}

impl StartupIsolateHandle {
    #[expect(
        clippy::expect_used,
        reason = "the private startup handle is consumed exactly once"
    )]
    fn into_inner(mut self) -> RuntimeTerminationHandle {
        self.0.take().expect("startup isolate handle")
    }
}

impl Drop for RuntimeTerminationHandle {
    fn drop(&mut self) {
        self.terminate_execution();
    }
}

async fn receive_runtime_startup<T>(
    receiver: tokio::sync::oneshot::Receiver<Result<T, String>>,
    timeout: Duration,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(value)) => value,
        Err(_) => Err(format!(
            "code mode runtime initialization exceeded its {}ms timeout",
            timeout.as_millis()
        )),
        Ok(Err(_)) => Err("failed to initialize code mode runtime".to_string()),
    }
}

fn report_runtime_panic(
    payload: Box<dyn std::any::Any + Send>,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    if let Some(task_failure_handler) = task_failure_handler {
        task_failure_handler(format!("code-mode V8 runtime thread panicked: {message}"));
    }
    let _ = event_tx.send(RuntimeEvent::ThreadPanicked);
}

struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Arc<EnabledToolCatalog>,
    source: String,
    stored_values: HashMap<String, StoredValue>,
    default_tool_timeout_ms: u64,
    output_admission: Arc<OutputAdmission>,
}

#[derive(Default)]
pub(crate) struct EnabledToolCatalog {
    tools: Vec<EnabledToolMetadata>,
    presentation_names: Vec<String>,
    namespace_aliases: BTreeMap<(String, String), usize>,
    by_global_name: HashMap<String, usize>,
    by_requested_name: HashMap<String, Option<usize>>,
    input_bytes: usize,
}

fn admit_short_aliases(
    candidates: &[Option<String>], registered: &HashMap<String, usize>,
    requested: &HashMap<String, Option<usize>>,
) -> Vec<Option<String>> {
    let mut counts = HashMap::new();
    for candidate in candidates.iter().flatten() {
        *counts.entry(candidate.as_str()).or_insert(0usize) += 1;
    }
    candidates.iter().enumerate().map(|(index, candidate)| {
        candidate.as_ref().filter(|name| counts.get(name.as_str()) == Some(&1)
            && registered.get(name.as_str()).is_none_or(|owner| *owner == index)
            && requested.get(name.as_str()).is_none_or(|owner| *owner == Some(index))).cloned()
    }).collect()
}

impl EnabledToolCatalog {
    pub(crate) fn from_definitions(
        definitions: &[codex_code_mode_protocol::ToolDefinition],
    ) -> Result<Self, String> {
        Self::new(
            definitions
                .iter()
                .map(|definition| EnabledToolMetadata {
                    global_name: normalize_code_mode_identifier(&definition.name),
                    tool_name: definition.tool_name.clone(),
                    description: Arc::clone(&definition.description),
                    namespace_description: definition.namespace_description.clone(),
                    kind: definition.kind,
                    default_timeout_ms: definition
                        .default_timeout_ms
                        .map(|timeout| timeout.min(codex_code_mode_protocol::MAX_TOOL_TIMEOUT_MS)),
                })
                .collect(),
        )
    }

    pub(crate) fn input_bytes(&self) -> usize {
        self.input_bytes
    }

    fn new(mut tools: Vec<EnabledToolMetadata>) -> Result<Self, String> {
        let mut by_global_name: HashMap<String, usize> = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            if let Some(existing_index) = by_global_name.get(&tool.global_name) {
                let existing = &tools[*existing_index];
                return Err(format!(
                    "code mode tool identifier collision: `{}` and `{}` both normalize to `{}`",
                    existing.tool_name, tool.tool_name, tool.global_name
                ));
            }
            by_global_name.insert(tool.global_name.clone(), index);
        }
        let mut by_requested_name = HashMap::new();
        for (index, tool) in tools.iter().enumerate() {
            let mut aliases = vec![tool.global_name.clone(), tool.tool_name.to_string()];
            if let Some(namespace) = &tool.tool_name.namespace
                && !namespace.contains('.')
            {
                aliases.push(format!("{namespace}.{}", tool.tool_name.name));
            }
            for alias in aliases {
                by_requested_name
                    .entry(alias)
                    .and_modify(|existing| {
                        if *existing != Some(index) {
                            *existing = None;
                        }
                    })
                    .or_insert(Some(index));
            }
        }
        let namespace_roots = tools.iter().flat_map(|tool| {
            tool.tool_name.namespace.as_deref().into_iter().chain(
                tool.global_name.split_once("__").filter(|(_, member)| !member.contains("__"))
                    .map(|(namespace, _)| namespace),
            )
        }).map(str::to_owned).collect::<HashSet<_>>();
        let candidates = tools.iter().map(|tool| {
            if tool.tool_name.namespace.is_none()
                && normalize_code_mode_identifier(&tool.tool_name.name) == tool.tool_name.name
            { return None; }
            // Only shorten our historical generated alias, never arbitrary
            // registration names. A digest prefix is presentation, not identity.
            if tool.global_name != codex_code_mode_protocol::legacy_code_mode_identifier(&tool.tool_name) {
                return None;
            }
            let (prefix, digest) = tool.global_name.rsplit_once("__")?;
            (digest.len() == 64).then(|| format!("{}__{}", &prefix[..prefix.len().min(24)], &digest[..12]))
                .filter(|alias| !namespace_roots.contains(alias))
        }).collect::<Vec<_>>();
        let short_aliases = admit_short_aliases(&candidates, &by_global_name, &by_requested_name);
        for (index, alias) in short_aliases.iter().enumerate() {
            if let Some(alias) = alias { by_requested_name.insert(alias.clone(), Some(index)); }
        }
        // Discovery and installation must admit precisely the same paths,
        // including collisions between registered identities and legacy names.
        let mut aliases = BTreeMap::new();
        for (index, tool) in tools.iter().enumerate() {
            let legacy = tool.global_name.split_once("__")
                .filter(|(_, member)| !member.contains("__"));
            let registered = tool.tool_name.namespace.as_deref()
                .map(|namespace| (namespace, tool.tool_name.name.as_str()));
            for (namespace, member) in legacy.into_iter().chain(registered) {
                if namespace.is_empty() || member.is_empty() { continue; }
                aliases.entry((namespace.to_owned(), member.to_owned()))
                    .and_modify(|owner| { if *owner != Some(index) { *owner = None; } })
                    .or_insert(Some(index));
            }
        }
        let namespace_aliases = aliases.into_iter().filter_map(|((namespace, member), owner)| {
            owner.filter(|_| !by_global_name.contains_key(&namespace))
                .map(|index| ((namespace, member), index))
        }).collect::<BTreeMap<_, _>>();
        let mut presentation_names = Vec::with_capacity(tools.len());
        for (index, tool) in tools.iter_mut().enumerate() {
            let mut presentation = tool.global_name.clone();
            if let Some(namespace) = &tool.tool_name.namespace {
                let member = &tool.tool_name.name;
                let safe = |name: &str| !name.is_empty()
                    && normalize_code_mode_identifier(name) == name;
                let qualified = format!("{namespace}.{member}");
                if safe(namespace) && safe(member)
                    && short_aliases[index].as_ref().is_none_or(|alias| qualified.len() <= alias.len())
                    && namespace_aliases.get(&(namespace.clone(), member.clone())) == Some(&index)
                    && by_requested_name.get(&qualified) == Some(&Some(index))
                {
                    // Rewrite only the generated declaration, not tool prose or
                    // canonical identity. Unknown contract layouts fail closed.
                    let needle = format!("declare const tools: {{ {}(", tool.global_name);
                    if let Some((before, rest)) = tool.description.rsplit_once(&needle)
                        && let Some((body, after)) = rest.rsplit_once(" };")
                    {
                        tool.description = format!("{before}declare const tools: {{ {namespace}: {{ {member}({body} }} }};{after}").into();
                        presentation = qualified;
                    }
                }
            }
            if presentation == tool.global_name
                && let Some(short) = &short_aliases[index]
            {
                let needle = format!("declare const tools: {{ {}(", tool.global_name);
                if let Some((before, rest)) = tool.description.rsplit_once(&needle) {
                    tool.description = format!("{before}declare const tools: {{ {short}({rest}").into();
                    presentation = short.clone();
                }
            }
            presentation_names.push(presentation);
        }
        let alias_bytes = by_requested_name.keys().fold(
            by_requested_name
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, Option<usize>)>()),
            |bytes, name| bytes.saturating_add(name.len()),
        );
        let input_bytes = tools.iter().fold(alias_bytes, |bytes, tool| {
            bytes
                .saturating_add(std::mem::size_of::<EnabledToolMetadata>())
                .saturating_add(tool.global_name.len().saturating_mul(2))
                .saturating_add(tool.tool_name.name.len())
                .saturating_add(tool.tool_name.namespace.as_ref().map_or(0, String::len))
                .saturating_add(tool.description.len())
                .saturating_add(tool.namespace_description.as_ref().map_or(0, |description| description.len()))
        });
        let input_bytes = presentation_names.iter().fold(input_bytes, |bytes, name|
            bytes.saturating_add(std::mem::size_of::<String>()).saturating_add(name.len()));
        let input_bytes = namespace_aliases.keys().fold(input_bytes, |bytes, (namespace, member)|
            bytes.saturating_add(std::mem::size_of::<((String, String), usize)>())
                .saturating_add(namespace.len()).saturating_add(member.len()));
        Ok(Self {
            tools,
            presentation_names,
            namespace_aliases,
            by_global_name,
            by_requested_name,
            input_bytes,
        })
    }

    fn get(&self, index: usize) -> Option<&EnabledToolMetadata> {
        self.tools.get(index)
    }

    fn presentation_name(&self, index: usize) -> &str {
        &self.presentation_names[index]
    }

    fn index_of(&self, global_name: &str) -> Option<usize> {
        self.by_global_name.get(global_name).copied()
    }

    /// Resolves a global name or the `namespace.name` identity that tool
    /// discovery reports for a namespaced tool. Ambiguous identities stay
    /// unresolved rather than dispatching to an arbitrary tool.
    fn resolve_requested_name(&self, requested_name: &str) -> Option<usize> {
        self.by_requested_name
            .get(requested_name)
            .copied()
            .flatten()
    }

    fn resolution_diagnostic(&self, requested: Option<&str>) -> serde_json::Value {
        let valid = requested.is_some_and(|name| !name.trim().is_empty() && name.len() <= 4096);
        let name = requested.filter(|_| valid).unwrap_or_default();
        let exact = self
            .tools
            .iter()
            .filter(|tool| {
                tool.tool_name.to_string() == name
                    || tool.tool_name.name == name
                    || name.split_once('.').is_some_and(|(namespace, local_name)| {
                        tool.tool_name.namespace.as_deref() == Some(namespace)
                            && tool.tool_name.name == local_name
                    })
            })
            .collect::<Vec<_>>();
        let ambiguous = exact.len() > 1;
        let candidates = if exact.is_empty() && valid {
            self.tools
                .iter()
                .filter(|tool| tool.global_name.contains(name))
                .collect::<Vec<_>>()
        } else {
            exact
        };
        let total = candidates.len();
        let mut bytes = 0;
        let candidates = candidates
            .into_iter()
            .filter_map(|tool| {
                let size = serde_json::to_string(&tool.global_name).ok()?.len();
                if bytes + size > 2048 {
                    return None;
                }
                bytes += size;
                Some(tool.global_name.clone())
            })
            .take(8)
            .collect::<Vec<_>>();
        serde_json::json!({
            "status": if !valid { "invalid_name" } else if ambiguous { "ambiguous" } else { "not_found" },
            "omitted_candidate_count": total.saturating_sub(candidates.len()),
            "candidates": candidates,
            "recovery": "Resolve an exact canonical candidate with resolve_tool(name); otherwise use tool_search for capability discovery. No tool was invoked."
        })
    }
}

pub(super) struct RuntimeState {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    monotonic_origin: std::time::Instant,
    pending_tool_calls: HashMap<String, (v8::Global<v8::PromiseResolver>, ToolName)>,
    output_projector: Option<v8::Global<v8::Function>>,
    pending_notifications: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    unhandled_rejections: Vec<v8::Global<v8::Promise>>,
    untracked_rejections: usize,
    stored_values: HashMap<String, StoredValue>,
    total_stored_value_bytes: usize,
    stored_value_writes: HashMap<String, StoredValue>,
    storage_limit_rejected: bool,
    stored_value_reads: Option<HashSet<String>>,
    enabled_tools: Arc<EnabledToolCatalog>,
    next_tool_call_id: u64,
    tool_functions: HashMap<usize, v8::Global<v8::Function>>,
    next_notification_id: u64,
    next_timeout_id: u64,
    default_tool_timeout_ms: u64,
    tool_call_id: String,
    timer_scheduler: timers::TimerScheduler,
    exit_requested: bool,
    output_admission: Arc<OutputAdmission>,
}

pub(crate) const MAX_OUTSTANDING_CALLBACKS_PER_CELL: usize = 128;
pub(crate) const MAX_SESSION_STORED_VALUES: usize = 256;
pub(crate) const MAX_SESSION_STORED_VALUE_BYTES: usize = 8 * 1024 * 1024;
/// Keys returned by one `listKeys` page when the caller does not ask.
pub(crate) const DEFAULT_LIST_KEYS_LIMIT: u64 = 32;
/// Largest `listKeys` page. A larger request is clamped to this and paged with
/// `next_after`, so the ceiling bounds one result without hiding any key.
/// Public so the tool contract can state the real number to the model.
pub const MAX_LIST_KEYS_LIMIT: u64 = 64;

/// Counts serialized bytes without retaining them.
#[derive(Default)]
pub(crate) struct JsonByteCounter {
    pub(crate) bytes: usize,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn stored_value_entry_bytes(key: &str, value: &JsonValue) -> usize {
    let mut counter = JsonByteCounter::default();
    if serde_json::to_writer(&mut counter, key).is_err()
        || serde_json::to_writer(&mut counter, value).is_err()
    {
        return usize::MAX;
    }
    counter.bytes
}

pub(crate) fn stored_values_with_writes_within_limits(
    current: &HashMap<String, StoredValue>,
    writes: &HashMap<String, StoredValue>,
) -> bool {
    let entry_count = current
        .keys()
        .filter(|key| !writes.contains_key(*key))
        .count()
        + writes.values().filter(|value| !value.deleted).count();
    if entry_count > MAX_SESSION_STORED_VALUES {
        return false;
    }

    current
        .iter()
        .filter(|(key, _)| !writes.contains_key(*key))
        .chain(writes.iter().filter(|(_, value)| !value.deleted))
        .try_fold(0usize, |total, (_, stored)| total.checked_add(stored.bytes))
        .is_some_and(|bytes| bytes <= MAX_SESSION_STORED_VALUE_BYTES)
}

pub(crate) fn apply_stored_value_writes(
    current: &mut HashMap<String, StoredValue>,
    writes: HashMap<String, StoredValue>,
) {
    for (key, mut value) in writes {
        if value.deleted {
            current.remove(&key);
        } else {
            value.read_dependencies = None;
            current.insert(key, value);
        }
    }
}

pub(crate) fn stored_value_limit_message() -> String {
    format!(
        "code mode session storage exceeds its limit of {MAX_SESSION_STORED_VALUES} entries or {MAX_SESSION_STORED_VALUE_BYTES} serialized bytes. Inspect listKeys().usage and paged entry_bytes for charged bytes without loading values; retire only obsolete keys with deleteStored. Preserve completed results; do not rerun their producers."
    )
}

impl RuntimeState {
    pub(super) fn can_admit_callback(&self) -> bool {
        self.pending_tool_calls.len() + self.pending_notifications.len()
            < MAX_OUTSTANDING_CALLBACKS_PER_CELL
    }

    pub(super) fn emit_output(&self, item: FunctionCallOutputContentItem) {
        if self.exit_requested {
            return;
        }
        if let Some(event) = self.output_admission.admit(item) {
            let _ = self.event_tx.send(event);
        }
    }

    pub(super) fn stored_value_completion(&mut self) -> HashMap<String, StoredValue> {
        std::mem::take(&mut self.stored_value_writes)
    }
}

pub(super) enum CompletionState {
    Pending,
    Completed {
        stored_value_writes: HashMap<String, StoredValue>,
        error_text: Option<String>,
    },
}

fn run_runtime(
    scope: &mut v8::PinScope<'_, '_>,
    projector: v8::Global<v8::Function>,
    config: RuntimeConfig,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    isolate_handle_tx: tokio::sync::oneshot::Sender<Result<StartupIsolateHandle, String>>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    if isolate_handle_tx.is_closed() {
        return;
    }
    let isolate_handle = scope.thread_safe_handle();
    if isolate_handle_tx
        .send(Ok(StartupIsolateHandle(Some(RuntimeTerminationHandle {
            isolate: isolate_handle,
            command_tx: runtime_command_tx.clone(),
        }))))
        .is_err()
    {
        return;
    }
    let timer_scheduler = timers::TimerScheduler::new(runtime_command_tx);
    let total_stored_value_bytes = config
        .stored_values
        .values()
        .fold(0usize, |total, stored| total.saturating_add(stored.bytes));
    scope.set_slot(RuntimeState {
        event_tx: event_tx.clone(),
        monotonic_origin: std::time::Instant::now(),
        pending_tool_calls: HashMap::new(),
        output_projector: Some(projector),
        pending_notifications: HashMap::new(),
        pending_timeouts: HashMap::new(),
        unhandled_rejections: Vec::new(),
        untracked_rejections: 0,
        stored_values: config.stored_values,
        total_stored_value_bytes,
        stored_value_writes: HashMap::new(),
        storage_limit_rejected: false,
        stored_value_reads: Some(HashSet::new()),
        enabled_tools: config.enabled_tools,
        next_tool_call_id: 1,
        tool_functions: HashMap::new(),
        next_notification_id: 1,
        next_timeout_id: 1,
        default_tool_timeout_ms: config.default_tool_timeout_ms,
        tool_call_id: config.tool_call_id,
        timer_scheduler,
        exit_requested: false,
        output_admission: config.output_admission,
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_result(scope, &event_tx, HashMap::new(), Some(error_text));
        return;
    }

    let _ = event_tx.send(RuntimeEvent::Started);

    let evaluation = {
        v8::scope!(let scope, scope);
        module_loader::evaluate_main_module(scope, &config.source)
    };
    let pending_promise = match evaluation {
        Ok(pending_promise) => pending_promise,
        Err(error_text) => {
            capture_scope_send_error(scope, &event_tx, Some(error_text));
            return;
        }
    };

    match module_loader::completion_state(scope, pending_promise.as_ref()) {
        CompletionState::Completed {
            stored_value_writes,
            error_text,
        } => {
            send_result(scope, &event_tx, stored_value_writes, error_text);
            return;
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Ok(command) = command_rx.recv() {
        v8::scope!(let scope, scope);
        match command {
            RuntimeCommand::Terminate => break,
            RuntimeCommand::ToolResponse { id, result } => {
                tracing::info!(event.name = "codex.code_mode_nested_tool.runtime_receive",
                    runtime_tool_call_id = %id, transport_outcome = "completed",
                    endpoint = "runtime_command_received", "runtime received nested result");
                if let Err(error_text) =
                    module_loader::resolve_tool_response(scope, &id, Ok(result), None)
                {
                    capture_scope_send_error(scope, &event_tx, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                tracing::info!(event.name = "codex.code_mode_nested_tool.runtime_receive",
                    runtime_tool_call_id = %id, transport_outcome = "failed",
                    endpoint = "runtime_command_received", "runtime received nested error");
                if let Err(runtime_error) =
                    module_loader::resolve_tool_response(scope, &id, Err(error_text), None)
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::NotificationResponse { id } => {
                if let Err(runtime_error) =
                    module_loader::resolve_notification_response(scope, &id, Ok(()))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::ToolResponseAfterDeadline { id, result, error_text } => {
                if let Err(runtime_error) = module_loader::resolve_tool_response(scope, &id, Err(error_text), Some(result)) {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::NotificationError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_notification_response(scope, &id, Err(error_text))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::TimersReady => {
                let fired = scope
                    .get_slot::<RuntimeState>()
                    .map(|state| state.timer_scheduler.take_fired())
                    .unwrap_or_default();
                for id in fired {
                    match timers::invoke_timeout_callback(scope, id) {
                        Ok(ControlFlow::Continue(())) => {}
                        Ok(ControlFlow::Break(())) => {
                            capture_scope_send_error(scope, &event_tx, None);
                            return;
                        }
                        Err(runtime_error) => {
                            capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                            return;
                        }
                    }
                    scope.perform_microtask_checkpoint();
                    if let CompletionState::Completed {
                        stored_value_writes,
                        error_text,
                    } = module_loader::completion_state(scope, pending_promise.as_ref())
                    {
                        send_result(scope, &event_tx, stored_value_writes, error_text);
                        return;
                    }
                }
            }
        }

        scope.perform_microtask_checkpoint();
        match module_loader::completion_state(scope, pending_promise.as_ref()) {
            CompletionState::Completed {
                stored_value_writes,
                error_text,
            } => {
                send_result(scope, &event_tx, stored_value_writes, error_text);
                return;
            }
            CompletionState::Pending => {}
        }

        if let Some(promise) = pending_promise.as_ref() {
            let promise = v8::Local::new(scope, promise);
            if promise.state() != v8::PromiseState::Pending {
                pending_promise = None;
            }
        }
    }
}

fn capture_scope_send_error(
    scope: &mut v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    error_text: Option<String>,
) {
    let stored_value_writes = scope
        .get_slot_mut::<RuntimeState>()
        .map(RuntimeState::stored_value_completion)
        .unwrap_or_default();

    send_result(scope, event_tx, stored_value_writes, error_text);
}

fn unawaited_tool_error(scope: &v8::PinScope<'_, '_>) -> Option<String> {
    scope.get_slot::<RuntimeState>().and_then(|state| {
        (!state.pending_tool_calls.is_empty()).then(|| {
            format!(
                "cell completed with {} unawaited tool call(s); outstanding tool work is cancelled",
                state.pending_tool_calls.len()
            )
        })
    })
}

fn send_result(
    scope: &v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    mut stored_value_writes: HashMap<String, StoredValue>,
    error_text: Option<String>,
) {
    // Timer exit bypasses completion_state. Every terminal path must reject
    // abandoned tools, including exit(), without replacing an earlier error.
    // Exception rendering may itself call exit(); do not turn that exit into
    // a formatting failure. Outstanding tools still retain their error below.
    let error_text = error_text.filter(|_| {
        !scope.get_slot::<RuntimeState>().is_some_and(|state| state.exit_requested)
    });
    let error_text = error_text.or_else(|| unawaited_tool_error(scope));
    // Catching an admission error and completing successfully can compact and
    // commit retained evidence. A failed quota-exceeding cell is transactional:
    // do not commit the prefix of its writes.
    if error_text.is_some()
        && scope
            .get_slot::<RuntimeState>()
            .is_some_and(|state| state.storage_limit_rejected)
    {
        stored_value_writes.clear();
    }
    if !stored_value_writes.is_empty() {
        let reads = scope
            .get_slot::<RuntimeState>()
            .and_then(|state| state.stored_value_reads.clone())
            .map(Arc::new);
        for stored in stored_value_writes.values_mut() {
            stored.read_dependencies = reads.clone();
        }
    }
    let error_text = error_text.map(|text| {
        if text.len() > MAX_ERROR_TEXT_BYTES {
            // Error frames stay small, but settled helper evidence belongs
            // in the existing bounded content/artifact path before clipping.
            if text.contains("\nHelper ")
                && let Some(state) = scope.get_slot::<RuntimeState>()
            {
                state.emit_output(FunctionCallOutputContentItem::InputText { text: text.clone() });
            }
            let mut end = MAX_ERROR_TEXT_BYTES - ERROR_TRUNCATION_SUFFIX.len();
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            // Drop the oversized backing buffer before notification drain.
            let mut bounded = String::with_capacity(MAX_ERROR_TEXT_BYTES);
            bounded.push_str(&text[..end]);
            bounded.push_str(ERROR_TRUNCATION_SUFFIX);
            return bounded;
        }
        text
    });
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        output_loss: scope
            .get_slot::<RuntimeState>()
            .and_then(|state| state.output_admission.output_loss()),
        error_text,
    });
}
