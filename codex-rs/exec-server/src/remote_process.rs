use std::sync::Arc;

use tokio::sync::watch;
use tracing::trace;

use crate::ExecBackend;
use crate::ExecBackendFuture;
use crate::ExecProcess;
use crate::ExecProcessEventReceiver;
use crate::ExecProcessFuture;
use crate::StartedExecProcess;
use crate::client::LazyRemoteExecServerClient;
use crate::client::Session;
use crate::protocol::ExecParams;
use crate::protocol::ProcessSignal;
use crate::protocol::ReadResponse;
use crate::protocol::WriteResponse;

#[derive(Clone)]
pub(crate) struct RemoteProcess {
    client: RemoteProcessClient,
}

#[derive(Clone)]
enum RemoteProcessClient {
    Lazy(LazyRemoteExecServerClient),
    Bound(crate::client::ExecServerClient),
}

struct RemoteExecProcess {
    session: Session,
}

impl RemoteProcess {
    pub(crate) fn new(client: LazyRemoteExecServerClient) -> Self {
        trace!("remote process new");
        Self {
            client: RemoteProcessClient::Lazy(client),
        }
    }

    pub(crate) fn bound(client: crate::client::ExecServerClient) -> Self {
        Self {
            client: RemoteProcessClient::Bound(client),
        }
    }

    async fn start(
        &self,
        params: ExecParams,
    ) -> Result<StartedExecProcess, crate::ExecServerError> {
        let deadline = tokio::time::Instant::now() + crate::client::PROCESS_START_TIMEOUT;
        let client = match &self.client {
            RemoteProcessClient::Lazy(client) => tokio::time::timeout_at(deadline, client.get())
                .await
                .map_err(|_| crate::ExecServerError::ProcessStartTimedOut)??,
            RemoteProcessClient::Bound(client) => client.clone(),
        };
        let session = client.start_process_before(params, deadline).await?;

        Ok(StartedExecProcess {
            process: Arc::new(RemoteExecProcess { session }),
        })
    }
}

impl ExecBackend for RemoteProcess {
    fn start(&self, params: ExecParams) -> ExecBackendFuture<'_> {
        Box::pin(RemoteProcess::start(self, params))
    }
}

impl RemoteExecProcess {
    async fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> Result<ReadResponse, crate::ExecServerError> {
        self.session.read(after_seq, max_bytes, wait_ms).await
    }

    async fn write(&self, chunk: Vec<u8>) -> Result<WriteResponse, crate::ExecServerError> {
        trace!("exec process write");
        self.session.write(chunk).await
    }

    async fn signal(&self, signal: ProcessSignal) -> Result<(), crate::ExecServerError> {
        trace!("exec process signal");
        self.session.signal(signal).await
    }

    async fn terminate(&self) -> Result<(), crate::ExecServerError> {
        trace!("exec process terminate");
        self.session.terminate().await
    }
}

impl ExecProcess for RemoteExecProcess {
    fn termination_is_unreachable(&self) -> bool {
        self.session.termination_is_unreachable()
    }

    fn record_unresolved_termination(&self, reason: &str) {
        self.session.record_unresolved_termination(reason);
    }
    fn process_id(&self) -> &crate::ProcessId {
        self.session.process_id()
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.session.subscribe_wake()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        self.session.subscribe_events()
    }

    fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(RemoteExecProcess::read(self, after_seq, max_bytes, wait_ms))
    }

    fn write(&self, chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(RemoteExecProcess::write(self, chunk))
    }

    fn signal(&self, signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(RemoteExecProcess::signal(self, signal))
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(RemoteExecProcess::terminate(self))
    }
}

impl Drop for RemoteExecProcess {
    fn drop(&mut self) {
        self.session.unregister();
    }
}
