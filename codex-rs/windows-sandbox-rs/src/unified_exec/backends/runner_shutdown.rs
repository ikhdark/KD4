//! Cancellation request admission is independent of native shutdown confirmation.
use std::io;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::Duration;

pub(crate) struct RunnerShutdown {
    receipt: Mutex<Receiver<bool>>,
    requested: bool,
    confirmed: bool,
}

impl RunnerShutdown {
    pub(crate) fn new(receipt: Receiver<bool>) -> Self {
        Self { receipt: Mutex::new(receipt), requested: false, confirmed: false }
    }

    pub(crate) fn confirm(&mut self, send: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if self.confirmed { return Ok(()); }
        let admission = if self.requested { Ok(()) } else { send() };
        let receipt = self.receipt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let confirmed = match admission {
            Ok(()) => {
                self.requested = true;
                receipt.recv_timeout(Duration::from_secs(5)).map_err(|error| {
                    io::Error::new(io::ErrorKind::TimedOut,
                        format!("runner shutdown uncertain; confirmation receipt unavailable: {error}"))
                })
            }
            // The reader publishes its receipt before closing outbound control.
            // A runner-side timeout may already have stopped the tree, or a
            // confirmation may race this send. Reuse that proof rather than
            // losing it behind a now-closed control pipe.
            Err(error) => receipt.try_recv().map_err(|_| error),
        }?;
        match confirmed {
            true => { self.confirmed = true; Ok(()) }
            false => Err(io::Error::other("runner reported root exit without process-tree shutdown confirmation")),
        }
    }
}
