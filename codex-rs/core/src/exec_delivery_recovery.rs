//! Opt-in, bounded raw suffix custody at the synchronous Windows handoff.
use super::*;

pub struct OutputDeltaRecovery {
    cap: usize,
    state: Mutex<RecoveryState>,
}

#[derive(Default)]
struct RecoveryState {
    stopped: bool,
    seen: [usize; 2],
    suffix: [Vec<u8>; 2],
}

impl OutputDeltaRecovery {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            state: Mutex::new(RecoveryState::default()),
        }
    }

    pub(super) fn publish(&self, stream: &StdoutStream, stderr: bool, chunk: Vec<u8>, admit: bool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = usize::from(stderr);
        let retained = self.cap.saturating_sub(state.seen[index]).min(chunk.len());
        state.seen[index] = state.seen[index].saturating_add(chunk.len());
        if chunk.is_empty() {
            return !state.stopped || state.seen[index] < self.cap;
        }
        if state.stopped || !admit {
            state.stopped = true;
            state.suffix[index].extend_from_slice(&chunk[..retained]);
            return state.seen[index] < self.cap;
        }
        let event = output_delta_event(stream, stderr, chunk);
        // Unlike the best-effort event helper, retain the rejected raw delta.
        if let Err(error) = stream.tx_event.try_send(event) {
            state.stopped = true;
            if let EventMsg::ExecCommandOutputDelta(delta) = error.into_inner().msg {
                state.suffix[index].extend_from_slice(&delta.chunk[..retained]);
            }
        }
        !state.stopped || state.seen[index] < self.cap
    }

    /// Called only after capture and the event sender have finished.
    pub fn take_suffix(&self) -> (Vec<u8>, Vec<u8>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let [stdout, stderr] = std::mem::take(&mut state.suffix);
        (stdout, stderr)
    }
}
