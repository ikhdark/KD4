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
        if !self.requested {
            send()?;
            self.requested = true;
        }
        match self.receipt.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(5)) {
            Ok(true) => { self.confirmed = true; Ok(()) }
            Ok(false) => Err(io::Error::other("runner reported root exit without process-tree shutdown confirmation")),
            Err(error) => Err(io::Error::new(io::ErrorKind::TimedOut,
                format!("runner shutdown uncertain; confirmation receipt unavailable: {error}"))),
        }
    }
}
