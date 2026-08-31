mod callbacks;
mod conversions;
mod types;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use self::callbacks::CallbackCompletion;
use self::callbacks::NotificationInvocation;
use self::callbacks::finish_callbacks;
use self::callbacks::report_task_result;
use self::callbacks::spawn_notification;
use self::callbacks::spawn_tool;
use self::conversions::cell_tool_kind;
use self::conversions::output_item;
use self::conversions::runtime_request;
use self::types::CellCommand;
pub(crate) use self::types::CellError;
pub(crate) use self::types::CellEventFuture;
pub(crate) use self::types::CellHandle;
pub(crate) use self::types::CellHost;
pub(crate) use self::types::CellState;
pub(crate) use self::types::CellToolCall;
pub(crate) use self::types::CompletionCommit;
use self::types::CompletionDelivery;
use self::types::ObservationDelivery;
use crate::TaskFailureHandler;
use crate::runtime::MAX_BUFFERED_OUTPUT_BYTES;
use crate::runtime::OutputAdmission;
use crate::runtime::RuntimeCommand;
use crate::runtime::RuntimeEvent;
use crate::runtime::spawn_runtime;
use crate::session_runtime::CellEvent;
use crate::session_runtime::CreateCellRequest as CellRequest;
use crate::session_runtime::ObserveMode;
use crate::session_runtime::OutputItem;
use crate::session_runtime::ToolName as CellToolName;

const STATE_CHANGE_COMPLETION_GRACE: Duration = Duration::from_millis(50);

pub(crate) struct CellActor;

impl CellActor {
    pub(crate) fn prepare<H: CellHost>(
        request: CellRequest,
        stored_values: HashMap<String, JsonValue>,
        host: Arc<H>,
        initial_observe_mode: ObserveMode,
        cell_state: Arc<CellState>,
        task_failure_handler: Option<TaskFailureHandler>,
    ) -> Result<
        (
            CellHandle,
            CellEventFuture,
            impl Future<Output = ()> + Send + 'static,
        ),
        String,
    > {
        let default_tool_timeout_ms = request.default_tool_timeout_ms;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (initial_response_tx, initial_response_rx) = oneshot::channel();
        let output_admission = Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES));
        let (runtime_tx, runtime_terminate_handle) = spawn_runtime(
            stored_values,
            runtime_request(request),
            default_tool_timeout_ms,
            event_tx,
            Arc::clone(&output_admission),
            task_failure_handler.clone(),
        )?;
        let handle = CellHandle::new(command_tx, Arc::clone(&cell_state));
        let task = run_cell(
            host,
            CellContext {
                runtime_tx,
                runtime_terminate_handle,
                cell_state,
                output_admission,
            },
            event_rx,
            command_rx,
            Observer {
                mode: initial_observe_mode,
                response_tx: initial_response_tx,
            },
            task_failure_handler,
        );
        let initial_response =
            Box::pin(async move { initial_response_rx.await.unwrap_or(Err(CellError::Closed)) });
        Ok((handle, initial_response, task))
    }
}

struct CellContext {
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_terminate_handle: v8::IsolateHandle,
    cell_state: Arc<CellState>,
    output_admission: Arc<OutputAdmission>,
}

struct Observer {
    mode: ObserveMode,
    response_tx: oneshot::Sender<Result<CellEvent, CellError>>,
}

async fn run_cell<H: CellHost>(
    host: Arc<H>,
    context: CellContext,
    mut event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    command_rx: mpsc::UnboundedReceiver<CellCommand>,
    initial_observer: Observer,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    let CellContext {
        runtime_tx,
        runtime_terminate_handle,
        cell_state,
        output_admission,
    } = context;
    let cancellation_token = cell_state.cancellation_token();
    let tool_cancellation_token = cancellation_token.child_token();
    let notification_cancellation_token = CancellationToken::new();
    let mut content_items = Vec::new();
    let mut admitted_output_bytes = 0usize;
    let mut observer = Some(initial_observer);
    let mut termination = false;
    let mut runtime_closed = false;
    let mut runtime_failure_reported = false;
    let mut yield_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut notification_tasks = JoinSet::new();
    let mut tool_tasks = JoinSet::new();
    let mut command_rx = Some(command_rx);
    loop {
        let yield_deadline_elapsed = yield_timer
            .as_ref()
            .is_some_and(|yield_timer| yield_timer.deadline() <= tokio::time::Instant::now());
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled(), if !termination => {
                termination = true;
                yield_timer = None;
                drop(command_rx.take());
                begin_termination(
                    &runtime_tx,
                    &runtime_terminate_handle,
                    &cancellation_token,
                );
                if runtime_closed {
                    finish_callbacks(
                        &notification_cancellation_token,
                        &tool_cancellation_token,
                        &mut notification_tasks,
                        &mut tool_tasks,
                        CallbackCompletion::Cancel,
                        task_failure_handler.as_ref(),
                    ).await;
                    finish_termination(
                        &cell_state,
                        observer.take().map(|observer| observer.response_tx),
                        CellEvent::Terminated {
                            content_items: std::mem::take(&mut content_items),
                        },
                    );
                    break;
                }
            }
            maybe_command = async {
                match command_rx.as_mut() {
                    Some(command_rx) => command_rx.recv().await,
                    None => std::future::pending::<Option<CellCommand>>().await,
                }
            } => {
                let Some(CellCommand::Observe { mode, response_tx }) = maybe_command else {
                    cancellation_token.cancel();
                    continue;
                };
                if response_tx.is_closed() {
                    continue;
                }
                let response_tx = match cell_state.route_observation(mode, response_tx) {
                    ObservationDelivery::Running(response_tx) => response_tx,
                    ObservationDelivery::Delivered => break,
                    ObservationDelivery::Buffered | ObservationDelivery::Closed => continue,
                };
                if observer
                    .as_ref()
                    .is_some_and(|observer| observer.response_tx.is_closed())
                {
                    observer = None;
                    yield_timer = None;
                }
                if observer.is_some() || termination {
                    let _ = response_tx.send(Err(CellError::Busy));
                    continue;
                }
                observer = Some(Observer { mode, response_tx });
                yield_timer = observer
                    .as_ref()
                    .and_then(|observer| observer_timer(observer, !content_items.is_empty()));
            }
            _ = async {
                if let Some(yield_timer) = yield_timer.as_mut() {
                    yield_timer.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                yield_timer = None;
                finish_yield_delivery(
                    send_observer_event(
                        observer.take(),
                        CellEvent::Yielded {
                            content_items: std::mem::take(&mut content_items),
                        },
                    ),
                    &mut content_items,
                    &mut admitted_output_bytes,
                    output_admission.as_ref(),
                );
            }
            maybe_event = async {
                if runtime_closed {
                    std::future::pending::<Option<RuntimeEvent>>().await
                } else {
                    event_rx.recv().await
                }
            }, if !yield_deadline_elapsed => {
                let Some(event) = maybe_event else {
                    runtime_closed = true;
                    if termination || cancellation_token.is_cancelled() {
                        finish_callbacks(
                            &notification_cancellation_token,
                            &tool_cancellation_token,
                            &mut notification_tasks,
                            &mut tool_tasks,
                            CallbackCompletion::Cancel,
                            task_failure_handler.as_ref(),
                        ).await;
                        finish_termination(
                            &cell_state,
                            observer.take().map(|observer| observer.response_tx),
                            CellEvent::Terminated {
                                content_items: std::mem::take(&mut content_items),
                            },
                        );
                        break;
                    }
                    if !runtime_failure_reported
                        && let Some(task_failure_handler) = &task_failure_handler
                    {
                        runtime_failure_reported = true;
                        task_failure_handler(
                            "code-mode V8 runtime thread ended unexpectedly".to_string(),
                        );
                    }
                    finish_callbacks(
                        &notification_cancellation_token,
                        &tool_cancellation_token,
                        &mut notification_tasks,
                        &mut tool_tasks,
                        CallbackCompletion::DrainNotifications,
                        task_failure_handler.as_ref(),
                    )
                    .await;
                    let event = CellEvent::Completed {
                        content_items: std::mem::take(&mut content_items),
                        error_text: Some("exec runtime ended unexpectedly".to_string()),
                    };
                    let rejected_event = match host
                        .commit_completion(
                            HashMap::new(),
                            event,
                            /*pending_initial_yield_items*/ None,
                            Arc::clone(&cell_state),
                        )
                        .await
                    {
                        CompletionCommit::Committed => None,
                        CompletionCommit::Rejected(event) => Some(event),
                    };
                    match cell_state.deliver_completion(
                        observer.take().map(|observer| observer.response_tx),
                    ) {
                        CompletionDelivery::Delivered => break,
                        CompletionDelivery::Buffered => {}
                        CompletionDelivery::Rejected(response_tx) => {
                            finish_termination(
                                &cell_state,
                                response_tx,
                                CellEvent::Terminated {
                                    content_items: rejected_completion_content(rejected_event),
                                },
                            );
                            break;
                        }
                    }
                    continue;
                };
                match event {
                    RuntimeEvent::Started => {
                        yield_timer = observer
                            .as_ref()
                            .and_then(|observer| observer_timer(observer, !content_items.is_empty()));
                    }
                    RuntimeEvent::ContentItem { item, admitted_bytes } => {
                        content_items.push(output_item(item));
                        admitted_output_bytes =
                            admitted_output_bytes.saturating_add(admitted_bytes);
                        if matches!(
                            observer.as_ref().map(|observer| observer.mode),
                            Some(ObserveMode::StateChange)
                        ) && yield_timer.is_none() {
                            // A tool result is commonly followed by cell completion in the
                            // same JavaScript turn. Give that terminal event a short chance
                            // to arrive so the owner can return one completed response instead
                            // of forcing another model turn solely to observe completion.
                            yield_timer = Some(Box::pin(tokio::time::sleep(
                                STATE_CHANGE_COMPLETION_GRACE,
                            )));
                        }
                    }
                    RuntimeEvent::YieldRequested => {
                        let yield_observer = matches!(
                            observer.as_ref().map(|observer| observer.mode),
                            Some(ObserveMode::YieldAfter(_) | ObserveMode::StateChange)
                        );
                        if yield_observer {
                            yield_timer = None;
                            finish_yield_delivery(
                                send_observer_event(
                                    observer.take(),
                                    CellEvent::Yielded {
                                        content_items: std::mem::take(&mut content_items),
                                    },
                                ),
                                &mut content_items,
                                &mut admitted_output_bytes,
                                output_admission.as_ref(),
                            );
                        }
                    }
                    RuntimeEvent::Notify { id, call_id, text } => {
                        spawn_notification(
                            &mut notification_tasks,
                            Arc::clone(&host),
                            NotificationInvocation { id, call_id, text },
                            runtime_tx.clone(),
                            notification_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    RuntimeEvent::ToolCall { id, name, kind, input, timeout_ms } => {
                        spawn_tool(
                            &mut tool_tasks,
                            Arc::clone(&host),
                            CellToolCall {
                                id,
                                name: CellToolName {
                                    name: name.name,
                                    namespace: name.namespace,
                                },
                                kind: cell_tool_kind(kind),
                                input,
                                timeout: std::time::Duration::from_millis(timeout_ms),
                            },
                            runtime_tx.clone(),
                            tool_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    RuntimeEvent::Result { stored_value_writes, error_text } => {
                        runtime_closed = true;
                        yield_timer = None;
                        if termination || cancellation_token.is_cancelled() {
                            finish_callbacks(
                                &notification_cancellation_token,
                                &tool_cancellation_token,
                                &mut notification_tasks,
                                &mut tool_tasks,
                                CallbackCompletion::Cancel,
                                task_failure_handler.as_ref(),
                            ).await;
                            finish_termination(
                                &cell_state,
                                observer.take().map(|observer| observer.response_tx),
                                CellEvent::Terminated {
                                    content_items: std::mem::take(&mut content_items),
                                },
                            );
                            break;
                        }
                        finish_callbacks(
                            &notification_cancellation_token,
                            &tool_cancellation_token,
                            &mut notification_tasks,
                            &mut tool_tasks,
                            CallbackCompletion::DrainNotifications,
                            task_failure_handler.as_ref(),
                        )
                        .await;
                        let event = CellEvent::Completed {
                            content_items: std::mem::take(&mut content_items),
                            error_text,
                        };
                        let rejected_event = match host
                            .commit_completion(
                                stored_value_writes,
                                event,
                                /*pending_initial_yield_items*/ None,
                                Arc::clone(&cell_state),
                            )
                            .await
                        {
                            CompletionCommit::Committed => None,
                            CompletionCommit::Rejected(event) => Some(event),
                        };
                        match cell_state.deliver_completion(
                            observer.take().map(|observer| observer.response_tx),
                        ) {
                            CompletionDelivery::Delivered => break,
                            CompletionDelivery::Buffered => {}
                            CompletionDelivery::Rejected(response_tx) => {
                                finish_termination(
                                    &cell_state,
                                    response_tx,
                                    CellEvent::Terminated {
                                        content_items: rejected_completion_content(rejected_event),
                                    },
                                );
                                break;
                            }
                        }
                    }
                    RuntimeEvent::ThreadPanicked => {
                        runtime_failure_reported = true;
                    }
                }
            }
            task_result = notification_tasks.join_next(), if !notification_tasks.is_empty() => {
                report_task_result(
                    task_result,
                    "notification",
                    task_failure_handler.as_ref(),
                );
            }
            task_result = tool_tasks.join_next(), if !tool_tasks.is_empty() => {
                report_task_result(task_result, "tool", task_failure_handler.as_ref());
            }
        }
    }
    // Reject requests that arrive while asynchronous terminal cleanup runs.
    cell_state.tombstone();
    drop(command_rx.take());
    begin_termination(&runtime_tx, &runtime_terminate_handle, &cancellation_token);
    finish_callbacks(
        &notification_cancellation_token,
        &tool_cancellation_token,
        &mut notification_tasks,
        &mut tool_tasks,
        CallbackCompletion::Cancel,
        task_failure_handler.as_ref(),
    )
    .await;
    host.closed(cell_state.terminal_event()).await;
}

fn send_observer_event(observer: Option<Observer>, event: CellEvent) -> Result<(), CellEvent> {
    let Some(observer) = observer else {
        return Err(event);
    };
    send_cell_event(observer.response_tx, event)
}

fn send_cell_event(
    response_tx: oneshot::Sender<Result<CellEvent, CellError>>,
    event: CellEvent,
) -> Result<(), CellEvent> {
    match response_tx.send(Ok(event)) {
        Ok(()) => Ok(()),
        Err(Ok(event)) => Err(event),
        Err(Err(error)) => panic!("cell event delivery returned an actor error: {error:?}"),
    }
}

fn finish_yield_delivery(
    delivery: Result<(), CellEvent>,
    content_items: &mut Vec<OutputItem>,
    admitted_output_bytes: &mut usize,
    output_admission: &OutputAdmission,
) {
    match delivery {
        Ok(()) => output_admission.release(std::mem::take(admitted_output_bytes)),
        Err(CellEvent::Yielded {
            content_items: mut undelivered_items,
        }) => {
            undelivered_items.append(content_items);
            *content_items = undelivered_items;
        }
        Err(event) => panic!("yield delivery returned an unexpected event: {event:?}"),
    }
}

fn rejected_completion_content(event: Option<CellEvent>) -> Vec<OutputItem> {
    match event {
        Some(CellEvent::Completed { content_items, .. }) => content_items,
        None => Vec::new(),
        Some(event) => panic!("completion commit rejected an unexpected event: {event:?}"),
    }
}

fn finish_termination(
    cell_state: &CellState,
    observer_tx: Option<oneshot::Sender<Result<CellEvent, CellError>>>,
    event: CellEvent,
) {
    if let Some(event) = cell_state.finish_termination(event)
        && let Some(observer_tx) = observer_tx
    {
        let _ = observer_tx.send(Ok(event));
    }
}

fn observer_timer(
    observer: &Observer,
    has_buffered_output: bool,
) -> Option<std::pin::Pin<Box<tokio::time::Sleep>>> {
    match observer.mode {
        ObserveMode::YieldAfter(duration) => Some(Box::pin(tokio::time::sleep(duration))),
        ObserveMode::StateChange if has_buffered_output => {
            Some(Box::pin(tokio::time::sleep(STATE_CHANGE_COMPLETION_GRACE)))
        }
        ObserveMode::StateChange => None,
    }
}

fn begin_termination(
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_terminate_handle: &v8::IsolateHandle,
    cancellation_token: &CancellationToken,
) {
    cancellation_token.cancel();
    let _ = runtime_tx.send(RuntimeCommand::Terminate);
    let _ = runtime_terminate_handle.terminate_execution();
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
