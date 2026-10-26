use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::RuntimeCommand;
use super::RuntimeState;
use super::module_loader::is_exit_exception;
use super::value::value_to_error_text;

const MAX_PENDING_TIMEOUTS_PER_CELL: usize = 128;

pub(super) struct ScheduledTimeout {
    callback: v8::Global<v8::Function>,
}

#[derive(Default)]
struct TimerWork {
    scheduled: HashMap<u64, Instant>,
    fired: Vec<u64>,
    wake_pending: bool,
    shutdown: bool,
}

impl TimerWork {
    fn queue_due(
        &mut self,
        now: Instant,
        tx: &std_mpsc::Sender<RuntimeCommand>,
    ) -> Result<(), String> {
        let mut due: Vec<_> = self
            .scheduled
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, deadline)| (*deadline, *id))
            .collect();
        due.sort_unstable();
        for (_, id) in due {
            self.scheduled.remove(&id);
            self.fired.push(id);
        }
        if !self.fired.is_empty() && !self.wake_pending {
            self.wake_pending = true;
            tx.send(RuntimeCommand::TimersReady)
                .map_err(|_| "timer runtime closed".to_string())?;
        }
        Ok(())
    }
}

pub(super) struct TimerScheduler {
    shared: Arc<(Mutex<TimerWork>, Condvar)>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TimerScheduler {
    pub(super) fn new(runtime_command_tx: std_mpsc::Sender<RuntimeCommand>) -> Self {
        Self {
            shared: Arc::new((Mutex::new(TimerWork::default()), Condvar::new())),
            runtime_command_tx,
            worker: None,
        }
    }

    fn schedule(&mut self, id: u64, delay: Duration) -> Result<(), String> {
        self.schedule_with_spawn(id, delay, |worker| thread::Builder::new().spawn(worker))
    }

    fn schedule_with_spawn(
        &mut self,
        id: u64,
        delay: Duration,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>>,
    ) -> Result<(), String> {
        let now = Instant::now();
        let deadline = now
            .checked_add(delay)
            .ok_or_else(|| "setTimeout delay exceeds the platform timer limit".to_string())?;
        if !delay.is_zero() && self.worker.is_none() {
            let shared = Arc::clone(&self.shared);
            let command_tx = self.runtime_command_tx.clone();
            self.worker = Some(spawn(Box::new(move || {
                run_scheduler(shared, command_tx);
            })).map_err(|error| format!("failed to start code mode timer worker: {error}"))?);
        }
        let (work, wake) = &*self.shared;
        let mut work = work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if work.scheduled.len() + work.fired.len() >= MAX_PENDING_TIMEOUTS_PER_CELL {
            return Err("code mode timer scheduler is full".to_string());
        }
        let previous = work.scheduled.values().min().copied();
        work.scheduled.insert(id, deadline);
        if delay.is_zero() {
            // Queue a normal runtime wake, never invoke JS synchronously. A
            // cell using only zero-delay timers needs no sleeping OS thread.
            work.queue_due(now, &self.runtime_command_tx)?;
        } else if previous.is_none_or(|previous| deadline < previous) {
            wake.notify_one();
        }
        Ok(())
    }

    fn cancel(&self, id: u64) {
        let (work, wake) = &*self.shared;
        let mut work = work
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = work.scheduled.values().min().copied();
        let removed = work.scheduled.remove(&id);
        work.fired.retain(|fired| *fired != id);
        if removed.is_some() && removed == previous {
            wake.notify_one();
        }
    }

    pub(super) fn take_fired(&self) -> Vec<u64> {
        let mut work = self
            .shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        work.wake_pending = false;
        std::mem::take(&mut work.fired)
    }
}

impl Drop for TimerScheduler {
    fn drop(&mut self) {
        let (work, wake) = &*self.shared;
        work.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown = true;
        wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_scheduler(
    shared: Arc<(Mutex<TimerWork>, Condvar)>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    let (work, wake) = &*shared;
    let mut work = work
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if work.shutdown {
            break;
        }
        // At most one wake is queued, even if JavaScript clears already-fired
        // timers and schedules replacements without returning to the event loop.
        if work.queue_due(Instant::now(), &runtime_command_tx).is_err() {
            break;
        }
        work = if let Some(deadline) = work.scheduled.values().min().copied() {
            wake.wait_timeout(work, deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
        } else {
            wake.wait(work)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
    }
}

pub(super) fn schedule_timeout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
) -> Result<Option<u64>, String> {
    let callback = args.get(0);
    if !callback.is_function() {
        return Err("setTimeout expects a function callback".to_string());
    }
    let callback = v8::Local::<v8::Function>::try_from(callback)
        .map_err(|_| "setTimeout expects a function callback".to_string())?;

    let Some(delay) = args.get(1).number_value(scope) else {
        // Preserve the pending coercion exception, without registering work.
        return Ok(None);
    };
    let delay_ms = normalize_delay_ms(delay);

    let callback = v8::Global::new(scope, callback);
    let state = scope
        .get_slot_mut::<RuntimeState>()
        .ok_or_else(|| "runtime state unavailable".to_string())?;
    if state.pending_timeouts.len() >= MAX_PENDING_TIMEOUTS_PER_CELL {
        return Err(format!(
            "code mode cell exceeded its limit of {MAX_PENDING_TIMEOUTS_PER_CELL} pending timers"
        ));
    }
    let timeout_id = state.next_timeout_id;
    state.next_timeout_id = state.next_timeout_id.saturating_add(1);
    state
        .pending_timeouts
        .insert(timeout_id, ScheduledTimeout { callback });
    if let Err(err) = state
        .timer_scheduler
        .schedule(timeout_id, Duration::from_millis(delay_ms))
    {
        state.pending_timeouts.remove(&timeout_id);
        return Err(err);
    }

    Ok(Some(timeout_id))
}

pub(super) fn clear_timeout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
) -> Result<(), String> {
    let Some(timeout_id) = timeout_id_from_args(scope, args)? else {
        return Ok(());
    };

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        return Err("runtime state unavailable".to_string());
    };
    if state.pending_timeouts.remove(&timeout_id).is_some() {
        state.timer_scheduler.cancel(timeout_id);
    }
    Ok(())
}

pub(super) fn invoke_timeout_callback(
    scope: &mut v8::PinScope<'_, '_>,
    timeout_id: u64,
) -> Result<ControlFlow<()>, String> {
    let callback = {
        let state = scope
            .get_slot_mut::<RuntimeState>()
            .ok_or_else(|| "runtime state unavailable".to_string())?;
        state.pending_timeouts.remove(&timeout_id)
    };
    let Some(callback) = callback else {
        return Ok(ControlFlow::Continue(()));
    };

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let callback = v8::Local::new(&tc, &callback.callback);
    let receiver = v8::undefined(&tc).into();
    let _ = callback.call(&tc, receiver, &[]);
    if tc.has_caught() {
        if let Some(exception) = tc.exception()
            && is_exit_exception(&mut tc, exception)
        {
            return Ok(ControlFlow::Break(()));
        }
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }

    Ok(ControlFlow::Continue(()))
}
fn timeout_id_from_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
) -> Result<Option<u64>, String> {
    if args.length() == 0 || args.get(0).is_null_or_undefined() {
        return Ok(None);
    }

    let Some(timeout_id) = args.get(0).number_value(scope) else {
        return Err("clearTimeout expects a numeric timeout id".to_string());
    };
    if !timeout_id.is_finite() || timeout_id <= 0.0 {
        return Ok(None);
    }

    Ok(Some(timeout_id.trunc().min(u64::MAX as f64) as u64))
}

fn normalize_delay_ms(delay_ms: f64) -> u64 {
    if !delay_ms.is_finite() || delay_ms <= 0.0 {
        0
    } else {
        delay_ms.trunc().min(u64::MAX as f64) as u64
    }
}
