use codex_exec_server_protocol::JSONRPCErrorError;

use crate::ExecServerRuntimePaths;
use crate::local_process::LocalProcess;
use crate::protocol::ExecParams;
use crate::protocol::ExecResponse;
use crate::protocol::ReadParams;
use crate::protocol::ReadResponse;
use crate::protocol::SignalParams;
use crate::protocol::SignalResponse;
use crate::protocol::TerminateParams;
use crate::protocol::TerminateResponse;
use crate::protocol::WriteParams;
use crate::protocol::WriteResponse;
use crate::rpc::RpcNotificationSender;
use crate::telemetry::ExecServerTelemetry;

#[derive(Clone)]
pub(crate) struct ProcessHandler {
    process: LocalProcess,
}

impl ProcessHandler {
    pub(crate) async fn reserve_owned_start(
        &self,
        params: &crate::protocol::OwnedProcessParams<ExecParams>,
    ) -> Result<crate::local_process::ProcessStartReservation, JSONRPCErrorError> {
        self.process
            .reserve_start_owned(&params.params.process_id, params.incarnation.clone())
            .await
    }

    pub(crate) async fn owned_identity(
        &self,
        id: &crate::ProcessId,
        incarnation: Option<&str>,
    ) -> Result<Option<crate::local_process::ProcessStartIdentity>, JSONRPCErrorError> {
        self.process.owned_identity(id, incarnation).await
    }

    pub(crate) async fn read_owned(
        &self,
        params: crate::protocol::OwnedProcessParams<ReadParams>,
    ) -> Result<ReadResponse, JSONRPCErrorError> {
        let identity = self
            .owned_identity(&params.params.process_id, params.incarnation.as_deref())
            .await?;
        self.process
            .exec_read_bound(params.params, identity.as_ref())
            .await
    }

    pub(crate) async fn write_owned(
        &self,
        params: crate::protocol::OwnedProcessParams<WriteParams>,
    ) -> Result<WriteResponse, JSONRPCErrorError> {
        let identity = self
            .owned_identity(&params.params.process_id, params.incarnation.as_deref())
            .await?;
        self.process
            .exec_write_bound(params.params, identity.as_ref())
            .await
    }

    pub(crate) async fn signal_owned(
        &self,
        params: crate::protocol::OwnedProcessParams<SignalParams>,
    ) -> Result<SignalResponse, JSONRPCErrorError> {
        let identity = self
            .owned_identity(&params.params.process_id, params.incarnation.as_deref())
            .await?;
        self.process
            .signal_process_bound(params.params, identity.as_ref())
            .await
    }

    pub(crate) async fn terminate_owned(
        &self,
        params: crate::protocol::OwnedProcessParams<TerminateParams>,
    ) -> Result<TerminateResponse, JSONRPCErrorError> {
        let Some(incarnation) = params.incarnation.as_deref() else {
            return self.process.terminate_admitted(params.params, None).await;
        };
        // Terminate is idempotent: an expired or replaced handle is not running
        // under its incarnation and must never control a successor generation.
        let Some(identity) = self
            .process
            .process_start_identity(&params.params.process_id)
            .await
            .filter(|identity| identity.has_incarnation(incarnation))
        else {
            return Ok(TerminateResponse { running: false });
        };
        self.process
            .terminate_admitted(params.params, Some(identity))
            .await
    }
    pub(crate) async fn process_start_identity(
        &self,
        process_id: &crate::ProcessId,
    ) -> Option<crate::local_process::ProcessStartIdentity> {
        self.process.process_start_identity(process_id).await
    }

    pub(crate) async fn exec_reserved(
        &self,
        params: ExecParams,
        reservation: crate::local_process::ProcessStartReservation,
    ) -> Result<ExecResponse, JSONRPCErrorError> {
        self.process.exec_reserved(params, reservation).await
    }

    pub(crate) async fn terminate_admitted(
        &self,
        params: TerminateParams,
        identity: crate::local_process::ProcessStartIdentity,
    ) -> Result<TerminateResponse, JSONRPCErrorError> {
        self.process
            .terminate_admitted(params, Some(identity))
            .await
    }
    pub(crate) fn new(
        notifications: RpcNotificationSender,
        telemetry: ExecServerTelemetry,
        runtime_paths: ExecServerRuntimePaths,
    ) -> Self {
        Self {
            process: LocalProcess::new(notifications, telemetry, runtime_paths),
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.process.shutdown().await;
    }


    pub(crate) fn set_notification_sender(&self, notifications: Option<RpcNotificationSender>) {
        self.process.set_notification_sender(notifications);
    }

    pub(crate) async fn exec(&self, params: ExecParams) -> Result<ExecResponse, JSONRPCErrorError> {
        self.process.exec(params).await
    }


}
