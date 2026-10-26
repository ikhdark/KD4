use std::time::Duration;

use tokio::time::Instant;

pub(crate) const WEBSOCKET_PONG_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const WEBSOCKET_PONG_TIMEOUT_REASON: &str = "pong_timeout";

/// Tracks whether a WebSocket peer has acknowledged a keepalive ping.
pub(crate) struct WebSocketPongWatchdog {
    timeout: Duration,
    deadline: Option<Instant>,
}

impl WebSocketPongWatchdog {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: None,
        }
    }

    pub(crate) fn ping_sent(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + self.timeout);
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn received_pong(&mut self) {
        self.deadline = None;
    }
}

/// Shared read/write ownership for ordinary, potentially 64 MiB WebSocket
/// messages. A Pong racing the Ping flush acknowledges that Ping as well.
pub(crate) struct WebSocketLiveness {
    state: std::sync::Mutex<(WebSocketPongWatchdog, u64)>,
    changed: tokio::sync::Notify,
}

impl WebSocketLiveness {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            state: std::sync::Mutex::new((WebSocketPongWatchdog::new(timeout), 0)),
            changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn pong_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }

    pub(crate) fn ping_flushed(&self, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.1 == generation {
            state.0.ping_sent(Instant::now());
        }
        self.changed.notify_one();
    }

    pub(crate) fn received_pong(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 = state.1.wrapping_add(1);
        state.0.received_pong();
        self.changed.notify_one();
    }

    pub(crate) async fn expired(&self) {
        loop {
            let changed = self.changed.notified();
            let deadline = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
                .deadline();
            tokio::select! {
                biased;
                _ = changed => {},
                _ = async { match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }} => return,
            }
        }
    }
}

