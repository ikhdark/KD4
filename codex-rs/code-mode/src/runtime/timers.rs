use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::RuntimeCommand;
use super::RuntimeState;
use super::value::value_to_error_text;

pub(super) struct ScheduledTimeout {
    callback: v8::Global<v8::Function>,
}

enum TimerSchedulerCommand {
    Schedule { id: u64, deadline: Instant },
    Cancel { id: u64 },
    Shutdown,
}

pub(super) struct TimerScheduler {
    command_tx: std_mpsc::Sender<TimerSchedulerCommand>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TimerScheduler {
    pub(super) fn new(runtime_command_tx: std_mpsc::Sender<RuntimeCommand>) -> Self {
        let (command_tx, command_rx) = std_mpsc::channel();
        let worker = thread::spawn(move || run_scheduler(command_rx, runtime_command_tx));
        Self {
            command_tx,
            worker: Some(worker),
        }
    }

    fn schedule(&self, id: u64, delay: Duration) -> Result<(), String> {
        let now = Instant::now();
        let deadline = now
            .checked_add(delay)
            .ok_or_else(|| "setTimeout delay exceeds the platform timer limit".to_string())?;
        self.command_tx
            .send(TimerSchedulerCommand::Schedule { id, deadline })
            .map_err(|_| "code mode timer scheduler is unavailable".to_string())
    }

    fn cancel(&self, id: u64) {
        let _ = self.command_tx.send(TimerSchedulerCommand::Cancel { id });
    }
}

impl Drop for TimerScheduler {
    fn drop(&mut self) {
        let _ = self.command_tx.send(TimerSchedulerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_scheduler(
    command_rx: std_mpsc::Receiver<TimerSchedulerCommand>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    let mut deadlines = BinaryHeap::<Reverse<(Instant, u64)>>::new();
    let mut scheduled = HashMap::<u64, Instant>::new();
    loop {
        while let Some(Reverse((deadline, id))) = deadlines.peek().copied() {
            if scheduled.get(&id) != Some(&deadline) {
                deadlines.pop();
            } else if deadline <= Instant::now() {
                deadlines.pop();
                scheduled.remove(&id);
                let _ = runtime_command_tx.send(RuntimeCommand::TimeoutFired { id });
            } else {
                break;
            }
        }

        let command = match deadlines.peek().copied() {
            Some(Reverse((deadline, _))) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                match command_rx.recv_timeout(wait) {
                    Ok(command) => command,
                    Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match command_rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        match command {
            TimerSchedulerCommand::Schedule { id, deadline } => {
                scheduled.insert(id, deadline);
                deadlines.push(Reverse((deadline, id)));
            }
            TimerSchedulerCommand::Cancel { id } => {
                scheduled.remove(&id);
            }
            TimerSchedulerCommand::Shutdown => break,
        }
    }
}

pub(super) fn schedule_timeout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
) -> Result<u64, String> {
    let callback = args.get(0);
    if !callback.is_function() {
        return Err("setTimeout expects a function callback".to_string());
    }
    let callback = v8::Local::<v8::Function>::try_from(callback)
        .map_err(|_| "setTimeout expects a function callback".to_string())?;

    let delay_ms = args
        .get(1)
        .number_value(scope)
        .map(normalize_delay_ms)
        .unwrap_or(0);

    let callback = v8::Global::new(scope, callback);
    let state = scope
        .get_slot_mut::<RuntimeState>()
        .ok_or_else(|| "runtime state unavailable".to_string())?;
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

    Ok(timeout_id)
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
) -> Result<(), String> {
    let callback = {
        let state = scope
            .get_slot_mut::<RuntimeState>()
            .ok_or_else(|| "runtime state unavailable".to_string())?;
        state.pending_timeouts.remove(&timeout_id)
    };
    let Some(callback) = callback else {
        return Ok(());
    };

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let callback = v8::Local::new(&tc, &callback.callback);
    let receiver = v8::undefined(&tc).into();
    let _ = callback.call(&tc, receiver, &[]);
    if tc.has_caught() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;
    use std::time::Instant;

    use super::TimerScheduler;

    #[test]
    fn dropping_scheduler_cancels_long_timers_without_waiting_for_their_deadlines() {
        let (runtime_tx, runtime_rx) = std_mpsc::channel();
        let started = Instant::now();
        let scheduler = TimerScheduler::new(runtime_tx);
        scheduler
            .schedule(1, Duration::from_secs(60))
            .expect("schedule long timer");
        scheduler
            .schedule(2, Duration::from_secs(60))
            .expect("schedule second long timer");
        scheduler.cancel(1);
        drop(scheduler);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            runtime_rx.recv_timeout(Duration::from_millis(20)),
            Err(std_mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn cancelled_timer_never_reaches_the_runtime() {
        let (runtime_tx, runtime_rx) = std_mpsc::channel();
        let scheduler = TimerScheduler::new(runtime_tx);
        scheduler
            .schedule(1, Duration::from_millis(5))
            .expect("schedule timer");
        scheduler.cancel(1);

        assert!(matches!(
            runtime_rx.recv_timeout(Duration::from_millis(20)),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        ));
        drop(scheduler);
        assert!(runtime_rx.try_recv().is_err());
    }
}
