use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use crate::client::ModelClientSession;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::session::Session;
use crate::session::turn::build_prompt;
use crate::session::turn::built_tools;
use crate::startup_timing::StartupPhase;
use crate::startup_timing::StartupTimingState;
use codex_otel::STARTUP_PREWARM_AGE_AT_FIRST_TURN_METRIC;
use codex_otel::STARTUP_PREWARM_DURATION_METRIC;
use codex_otel::SessionTelemetry;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;

pub(crate) struct SessionStartupPrewarmHandle {
    task: AbortOnDropHandle<CodexResult<ModelClientSession>>,
    started_at: Instant,
    timeout: Duration,
}

pub(crate) struct SessionStartupTransportHandle {
    task: AbortOnDropHandle<CodexResult<ModelClientSession>>,
}

enum SessionStartupTransportResolution {
    Ready(Box<ModelClientSession>),
    Unavailable,
}

pub(crate) enum SessionStartupPrewarmResolution {
    Cancelled,
    Ready(Box<ModelClientSession>),
    Unavailable {
        status: &'static str,
        prewarm_duration: Option<Duration>,
    },
}

impl SessionStartupPrewarmHandle {
    pub(crate) fn new(
        task: JoinHandle<CodexResult<ModelClientSession>>,
        started_at: Instant,
        timeout: Duration,
    ) -> Self {
        Self {
            task: AbortOnDropHandle::new(task),
            started_at,
            timeout,
        }
    }

    pub(crate) async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    #[instrument(name = "startup_prewarm.resolve", level = "trace", skip_all)]
    async fn resolve(
        self,
        session_telemetry: &SessionTelemetry,
        startup_timing: &Arc<StartupTimingState>,
        cancellation_token: &CancellationToken,
    ) -> SessionStartupPrewarmResolution {
        let _startup_wait = startup_timing.begin_phase(StartupPhase::FirstTurnPrewarmWait);
        let resolve_started_at = Instant::now();
        let Self {
            mut task,
            started_at,
            timeout,
        } = self;
        let age_at_first_turn = started_at.elapsed();
        let remaining = timeout.saturating_sub(age_at_first_turn);

        let resolution = if task.is_finished() {
            Self::resolution_from_join_result(task.await, started_at)
        } else {
            match tokio::select! {
                _ = cancellation_token.cancelled() => None,
                result = tokio::time::timeout(remaining, &mut task) => Some(result),
            } {
                Some(Ok(result)) => Self::resolution_from_join_result(result, started_at),
                Some(Err(_elapsed)) => {
                    task.abort();
                    info!("startup websocket prewarm timed out before the first turn could use it");
                    SessionStartupPrewarmResolution::Unavailable {
                        status: "timed_out",
                        prewarm_duration: Some(started_at.elapsed()),
                    }
                }
                None => {
                    task.abort();
                    session_telemetry.record_startup_phase(
                        "startup_prewarm_resolve",
                        resolve_started_at.elapsed(),
                        Some("cancelled"),
                    );
                    session_telemetry.record_duration(
                        STARTUP_PREWARM_AGE_AT_FIRST_TURN_METRIC,
                        age_at_first_turn,
                        &[("status", "cancelled")],
                    );
                    session_telemetry.record_duration(
                        STARTUP_PREWARM_DURATION_METRIC,
                        started_at.elapsed(),
                        &[("status", "cancelled")],
                    );
                    return SessionStartupPrewarmResolution::Cancelled;
                }
            }
        };
        let status = match &resolution {
            SessionStartupPrewarmResolution::Cancelled => "cancelled",
            SessionStartupPrewarmResolution::Ready(_) => "ready",
            SessionStartupPrewarmResolution::Unavailable { status, .. } => status,
        };
        startup_timing.record_prewarm_status(status);
        session_telemetry.record_startup_phase(
            "startup_prewarm_resolve",
            resolve_started_at.elapsed(),
            Some(status),
        );

        match resolution {
            SessionStartupPrewarmResolution::Cancelled => {
                SessionStartupPrewarmResolution::Cancelled
            }
            SessionStartupPrewarmResolution::Ready(prewarmed_session) => {
                session_telemetry.record_duration(
                    STARTUP_PREWARM_AGE_AT_FIRST_TURN_METRIC,
                    age_at_first_turn,
                    &[("status", "consumed")],
                );
                SessionStartupPrewarmResolution::Ready(prewarmed_session)
            }
            SessionStartupPrewarmResolution::Unavailable {
                status,
                prewarm_duration,
            } => {
                session_telemetry.record_duration(
                    STARTUP_PREWARM_AGE_AT_FIRST_TURN_METRIC,
                    age_at_first_turn,
                    &[("status", status)],
                );
                if let Some(prewarm_duration) = prewarm_duration {
                    session_telemetry.record_duration(
                        STARTUP_PREWARM_DURATION_METRIC,
                        prewarm_duration,
                        &[("status", status)],
                    );
                }
                SessionStartupPrewarmResolution::Unavailable {
                    status,
                    prewarm_duration,
                }
            }
        }
    }

    fn resolution_from_join_result(
        result: std::result::Result<CodexResult<ModelClientSession>, tokio::task::JoinError>,
        started_at: Instant,
    ) -> SessionStartupPrewarmResolution {
        match result {
            Ok(Ok(prewarmed_session)) => {
                SessionStartupPrewarmResolution::Ready(Box::new(prewarmed_session))
            }
            Ok(Err(err)) => {
                warn!("startup websocket prewarm setup failed: {err:#}");
                SessionStartupPrewarmResolution::Unavailable {
                    status: "failed",
                    prewarm_duration: None,
                }
            }
            Err(err) => {
                warn!("startup websocket prewarm setup join failed: {err}");
                SessionStartupPrewarmResolution::Unavailable {
                    status: "join_failed",
                    prewarm_duration: Some(started_at.elapsed()),
                }
            }
        }
    }
}

impl SessionStartupTransportHandle {
    fn new(task: JoinHandle<CodexResult<ModelClientSession>>) -> Self {
        Self {
            task: AbortOnDropHandle::new(task),
        }
    }

    async fn resolve(self) -> SessionStartupTransportResolution {
        match self.task.await {
            Ok(Ok(client_session)) => {
                SessionStartupTransportResolution::Ready(Box::new(client_session))
            }
            Ok(Err(err)) => {
                warn!(
                    "startup websocket preconnect failed; continuing with send-boundary fallback: {err:#}"
                );
                SessionStartupTransportResolution::Unavailable
            }
            Err(err) => {
                warn!(
                    "startup websocket preconnect join failed; continuing with send-boundary fallback: {err}"
                );
                SessionStartupTransportResolution::Unavailable
            }
        }
    }
}

impl Session {
    /// Begin transport-only setup as soon as the provider, auth identity, endpoint, and
    /// transport configuration are stable. Tool and prompt construction intentionally do
    /// not participate in this key or gate this work.
    pub(crate) async fn schedule_startup_transport_preconnect(self: &Arc<Self>) {
        if !crate::latency_switches::stage2_critical_path_enabled() {
            return;
        }
        if !self.services.model_client.responses_websocket_enabled() {
            let model_client = self.services.model_client.clone();
            tokio::spawn(async move {
                if let Err(err) = model_client.prewarm_auth().await {
                    warn!("startup auth prewarm failed: {err:#}");
                }
            });
            return;
        }

        let model_client = self.services.model_client.clone();
        let session_telemetry = self.services.session_telemetry.clone();
        let startup_timing = Arc::clone(&self.startup_timing);
        let responses_metadata = CodexResponsesMetadata::new(
            self.installation_id.clone(),
            self.thread_id.to_string(),
            self.thread_id.to_string(),
            self.current_window_id().await,
        );
        let task = tokio::spawn(async move {
            let _preconnect = startup_timing.begin_phase(StartupPhase::TransportPreconnect);
            let mut client_session = model_client.new_session();
            let result = client_session
                .preconnect_websocket(&session_telemetry, &responses_metadata)
                .await;
            startup_timing.record_prewarm_status(if result.is_ok() {
                "transport_ready"
            } else {
                "transport_failed"
            });
            if let Err(err) = result {
                return Err(CodexErr::Stream(
                    format!("startup websocket preconnect failed: {err}"),
                    None,
                ));
            }
            Ok(client_session)
        });
        self.set_session_startup_transport(SessionStartupTransportHandle::new(task))
            .await;
    }

    pub(crate) async fn schedule_startup_prewarm(self: &Arc<Self>, base_instructions: String) {
        if !self.services.model_client.responses_websocket_enabled() {
            return;
        }

        let session_telemetry = self.services.session_telemetry.clone();
        let websocket_connect_timeout = self.provider().await.websocket_connect_timeout();
        let started_at = Instant::now();
        let startup_prewarm_session = Arc::clone(self);
        let startup_transport = self.take_session_startup_transport().await;
        let startup_prewarm = tokio::spawn(async move {
            let preconnected_session = match startup_transport {
                Some(startup_transport) => match startup_transport.resolve().await {
                    SessionStartupTransportResolution::Ready(client_session) => {
                        Some(*client_session)
                    }
                    SessionStartupTransportResolution::Unavailable => {
                        return Err(CodexErr::Stream(
                            "startup websocket preconnect was unavailable; deferring transport retry to the first send boundary"
                                .to_string(),
                            None,
                        ));
                    }
                },
                None => None,
            };
            let result = schedule_startup_prewarm_inner(
                startup_prewarm_session,
                base_instructions,
                preconnected_session,
            )
            .await;
            let status = if result.is_ok() { "ready" } else { "failed" };
            session_telemetry.record_startup_phase(
                "startup_prewarm_total",
                started_at.elapsed(),
                Some(status),
            );
            session_telemetry.record_duration(
                STARTUP_PREWARM_DURATION_METRIC,
                started_at.elapsed(),
                &[("status", status)],
            );
            result
        });
        self.set_session_startup_prewarm(SessionStartupPrewarmHandle::new(
            startup_prewarm,
            started_at,
            websocket_connect_timeout,
        ))
        .await;
    }

    pub(crate) async fn consume_startup_prewarm_for_regular_turn(
        &self,
        cancellation_token: &CancellationToken,
    ) -> SessionStartupPrewarmResolution {
        let Some(startup_prewarm) = self.take_session_startup_prewarm().await else {
            return SessionStartupPrewarmResolution::Unavailable {
                status: "not_scheduled",
                prewarm_duration: None,
            };
        };
        startup_prewarm
            .resolve(
                &self.services.session_telemetry,
                &self.startup_timing,
                cancellation_token,
            )
            .await
    }
}

async fn schedule_startup_prewarm_inner(
    session: Arc<Session>,
    base_instructions: String,
    preconnected_session: Option<ModelClientSession>,
) -> CodexResult<ModelClientSession> {
    let _preparation = session
        .startup_timing
        .begin_phase(StartupPhase::PrewarmPreparation);
    let prewarm_started_at = Instant::now();
    let startup_turn_context = session
        .new_startup_prewarm_turn_with_sub_id(INITIAL_SUBMIT_ID.to_owned())
        .await;
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_create_turn_context",
        prewarm_started_at.elapsed(),
        /*status*/ None,
    );
    let startup_cancellation_token = CancellationToken::new();
    let built_tools_started_at = Instant::now();
    // Startup prewarm runs before run_turn and needs its own tool-building snapshot.
    let step_context = session
        .capture_step_context(Arc::clone(&startup_turn_context))
        .await;
    let startup_router = built_tools(
        session.as_ref(),
        step_context.as_ref(),
        &startup_cancellation_token,
    )
    .await?;
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_build_tools",
        built_tools_started_at.elapsed(),
        /*status*/ None,
    );
    let build_prompt_started_at = Instant::now();
    let startup_prompt = build_prompt(
        Vec::new(),
        startup_router.as_ref(),
        startup_turn_context.as_ref(),
        BaseInstructions {
            text: base_instructions,
        },
    );
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_build_prompt",
        build_prompt_started_at.elapsed(),
        /*status*/ None,
    );
    let window_id = session.current_window_id().await;
    let responses_metadata = startup_turn_context
        .turn_metadata_state
        .to_responses_metadata(
            session.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Prewarm,
        );
    let mut client_session =
        preconnected_session.unwrap_or_else(|| session.services.model_client.new_session());
    let websocket_warmup_started_at = Instant::now();
    drop(_preparation);
    let _prewarm_request = session
        .startup_timing
        .begin_phase(StartupPhase::PrewarmRequest);
    client_session
        .prewarm_websocket(
            &startup_prompt,
            &startup_turn_context.model_info,
            &startup_turn_context.session_telemetry,
            startup_turn_context.reasoning_effort.clone(),
            startup_turn_context.reasoning_summary,
            startup_turn_context.config.service_tier.clone(),
            &responses_metadata,
        )
        .await?;
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_websocket_warmup",
        websocket_warmup_started_at.elapsed(),
        /*status*/ None,
    );
    Ok(client_session)
}
