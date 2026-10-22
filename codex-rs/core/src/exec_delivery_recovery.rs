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

    pub(super) fn publish(&self, stream: &StdoutStream, stderr: bool, chunk: Vec<u8>, admit: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = usize::from(stderr);
        let retained = self.cap.saturating_sub(state.seen[index]).min(chunk.len());
        state.seen[index] = state.seen[index].saturating_add(chunk.len());
        if chunk.is_empty() {
            return;
        }
        if state.stopped || !admit {
            state.stopped = true;
            state.suffix[index].extend_from_slice(&chunk[..retained]);
            return;
        }
        let event = output_delta_event(stream, stderr, chunk);
        // Unlike the best-effort event helper, retain the rejected raw delta.
        if let Err(error) = stream.tx_event.try_send(event) {
            state.stopped = true;
            if let EventMsg::ExecCommandOutputDelta(delta) = error.into_inner().msg {
                state.suffix[index].extend_from_slice(&delta.chunk[..retained]);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturated_first_handoff_retains_raw_suffix_once() {
        let (tx, rx) = async_channel::bounded(1);
        let (stream, recovery) =
            StdoutStream::with_delivery_recovery(String::new(), String::new(), tx, 64);
        let limiter = OutputDeltaLimiter::default();
        for (stderr, bytes) in [
            (false, vec![b'A', 0xe2]),
            (false, vec![0x82, 0xac]),
            (true, b"diagnostic".to_vec()),
            (false, b"tail".to_vec()),
        ] {
            assert!(matches!(
                try_send_limited_output_delta(&stream, stderr, bytes, &limiter),
                OutputDeltaSendOutcome::Continue
            ));
        }
        let EventMsg::ExecCommandOutputDelta(first) = rx.try_recv().unwrap().msg else {
            panic!("delta")
        };
        let (suffix, stderr) = recovery.take_suffix();
        assert_eq!([first.chunk, suffix].concat(), "A€tail".as_bytes());
        assert_eq!(stderr, b"diagnostic");
        assert!(rx.is_empty());
        assert_eq!(recovery.take_suffix(), (Vec::new(), Vec::new()));
    }
}
