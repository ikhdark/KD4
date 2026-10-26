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
use crate::config::Config;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::session::Session;
use crate::session::turn::built_tools;
use crate::startup_timing::StartupPhase;
use crate::startup_timing::StartupTimingState;
use crate::tools::router::ToolRouter;
use codex_features::Feature;
use codex_otel::STARTUP_PREWARM_AGE_AT_FIRST_TURN_METRIC;
use codex_otel::STARTUP_PREWARM_DURATION_METRIC;
use codex_otel::SessionTelemetry;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

const CODE_MODE_PREWARM_PHASE: &str = "code_mode_prewarm";

pub(crate) struct SessionStartupPrewarmHandle {
    task: AbortOnDropHandle<CodexResult<Option<ModelClientSession>>>,
    started_at: Instant,
}

pub(crate) struct SessionStartupTransportHandle {
    task: AbortOnDropHandle<CodexResult<ModelClientSession>>,
}

pub(crate) struct PreparedStartupRouter {
    pub(crate) planning_generation: u64,
    pub(crate) config: Arc<Config>,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) router: Arc<ToolRouter>,
}

enum StartupPreparedRouterState {
    Open(Option<PreparedStartupRouter>),
    Closed,
}

/// One-shot handoff from speculative startup preparation to the first real turn.
///
/// The first plan attempt closes the handoff even when startup lost the race, so
/// a late speculative result can never leak into a later turn.
pub(crate) struct StartupPreparedRouterCache {
    state: tokio::sync::Mutex<StartupPreparedRouterState>,
}

impl Default for StartupPreparedRouterCache {
    fn default() -> Self {
        Self {
            state: tokio::sync::Mutex::new(StartupPreparedRouterState::Open(None)),
        }
    }
}

impl StartupPreparedRouterCache {
    pub(crate) async fn publish(&self, prepared: PreparedStartupRouter) {
        let mut state = self.state.lock().await;
        if let StartupPreparedRouterState::Open(slot) = &mut *state {
            *slot = Some(prepared);
        }
    }

    pub(crate) async fn take_for_first_turn(&self) -> Option<PreparedStartupRouter> {
        let mut state = self.state.lock().await;
        match std::mem::replace(&mut *state, StartupPreparedRouterState::Closed) {
            StartupPreparedRouterState::Open(prepared) => prepared,
            StartupPreparedRouterState::Closed => None,
        }
    }
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
        task: JoinHandle<CodexResult<Option<ModelClientSession>>>,
        started_at: Instant,
    ) -> Self {
        Self {
            task: AbortOnDropHandle::new(task),
            started_at,
        }
    }

    pub(crate) async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    /// Whether first-turn handoff can still claim this transport. `resolve` cuts off an
    /// unfinished prewarm, so a turn must not rely on it to supply the first socket.
    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    #[instrument(name = "startup_prewarm.resolve", level = "trace", skip_all)]
    pub(crate) async fn resolve(
        self,
        session_telemetry: &SessionTelemetry,
        startup_timing: &Arc<StartupTimingState>,
        cancellation_token: &CancellationToken,
    ) -> SessionStartupPrewarmResolution {
        let _startup_wait = startup_timing.begin_phase(StartupPhase::FirstTurnPrewarmWait);
        let resolve_started_at = Instant::now();
        let Self { task, started_at } = self;
        let age_at_first_turn = started_at.elapsed();

        let resolution = if cancellation_token.is_cancelled() {
            startup_timing.cutoff_speculative_prewarm("cancelled");
            task.abort();
            let _ = task.await;
            SessionStartupPrewarmResolution::Cancelled
        } else if task.is_finished() {
            Self::resolution_from_join_result(task.await, started_at)
        } else {
            // A speculative startup request must never delay ordinary dispatch. Reuse it only
            // when it has already completed; otherwise the regular turn owns the transport.
            startup_timing.cutoff_speculative_prewarm("not_ready");
            task.abort();
            drop(task);
            info!("startup websocket prewarm was not ready at first-turn handoff");
            SessionStartupPrewarmResolution::Unavailable {
                status: "not_ready",
                prewarm_duration: Some(started_at.elapsed()),
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
        result: std::result::Result<
            CodexResult<Option<ModelClientSession>>,
            tokio::task::JoinError,
        >,
        started_at: Instant,
    ) -> SessionStartupPrewarmResolution {
        match result {
            Ok(Ok(Some(prewarmed_session))) => {
                SessionStartupPrewarmResolution::Ready(Box::new(prewarmed_session))
            }
            Ok(Ok(None)) => SessionStartupPrewarmResolution::Unavailable {
                status: "disabled",
                prewarm_duration: None,
            },
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

    pub(crate) async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
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
        let cancellation = self.startup_cancellation.clone();
        if !self.services.model_client.startup_websocket_enabled() {
            let model_client = self.services.model_client.clone();
            let task = tokio::spawn(async move {
                let auth = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                    auth = model_client.prewarm_auth() => auth,
                };
                if let Err(err) = auth {
                    warn!("startup auth prewarm failed: {err:#}");
                }
                Ok(model_client.new_speculative_session())
            });
            self.set_session_startup_transport(SessionStartupTransportHandle::new(task))
                .await;
            return;
        }

        let model_client = self.services.model_client.clone();
        let session_telemetry = self.services.session_telemetry.clone();
        let startup_timing = Arc::clone(&self.startup_timing);
        let config = self.get_config().await;
        let thread_config = self.thread_config_snapshot().await;
        let models_manager = Arc::clone(&self.services.models_manager);
        let responses_metadata = CodexResponsesMetadata::new(
            self.installation_id.clone(),
            self.thread_id.to_string(),
            self.thread_id.to_string(),
            self.current_window_id().await,
        );
        let task = tokio::spawn(async move {
            let _preconnect = startup_timing.begin_phase(StartupPhase::TransportPreconnect);
            // Resolve routing without building a turn context or waiting for tool discovery.
            // The thread snapshot includes the tier inherited by spawned agents.
            let model_info = models_manager
                .get_model_info(&thread_config.model, &config.to_models_manager_config())
                .await;
            let service_tier = crate::session::get_service_tier(
                thread_config.service_tier,
                config.features.enabled(Feature::FastMode),
                &model_info,
            );
            let mut client_session = model_client.new_speculative_session();
            client_session.set_startup_timing(Arc::clone(&startup_timing));
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                result = client_session
                .preconnect_websocket(
                    &model_info,
                    service_tier,
                    &session_telemetry,
                    &responses_metadata,
                )
                => result,
            };
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
        let session_telemetry = self.services.session_telemetry.clone();
        let started_at = Instant::now();
        let startup_prewarm_session = Arc::clone(self);
        let cancellation = self.startup_cancellation.clone();
        // HTTP authentication remains session-owned until shutdown. Preparing a
        // router must neither wait for it nor abort it by dropping its handle.
        let startup_transport = if self.services.model_client.startup_websocket_enabled() {
            self.take_session_startup_transport().await
        } else {
            None
        };
        let startup_prewarm = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Ok(None),
                result = schedule_startup_prewarm_inner(
                startup_prewarm_session,
                base_instructions,
                startup_transport,
            )
                => result,
            };
            let status = match &result {
                Ok(Some(_)) => "ready",
                Ok(None) => "disabled",
                Err(_) => "failed",
            };
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
        ))
        .await;
    }

    /// Warms the code-mode host in the background so the first cell of the
    /// first turn does not wait for isolate and host startup. This is
    /// independent of tool-router and model prewarm above.
    pub(crate) fn schedule_code_mode_prewarm(
        self: &Arc<Self>,
        turn: &crate::session::turn_context::TurnContext,
    ) {
        if !code_mode_prewarm_enabled(turn) {
            return;
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let started_at = Instant::now();
            let result = tokio::select! {
                biased;
                _ = session.startup_cancellation.cancelled() => return,
                result = session.services.code_mode_service.prewarm() => result,
            };
            let status = if result.is_ok() { "ready" } else { "failed" };
            session.services.session_telemetry.record_startup_phase(
                CODE_MODE_PREWARM_PHASE,
                started_at.elapsed(),
                Some(status),
            );
            if let Err(error) = result {
                warn!(%error, "code-mode host prewarm failed; the first cell will start it");
            }
        });
    }

}

async fn resolve_startup_transport(
    startup_transport: Option<SessionStartupTransportHandle>,
) -> Option<ModelClientSession> {
    match startup_transport {
        Some(startup_transport) => match startup_transport.resolve().await {
            SessionStartupTransportResolution::Ready(client_session) => Some(*client_session),
            SessionStartupTransportResolution::Unavailable => None,
        },
        None => None,
    }
}

fn code_mode_prewarm_enabled(turn: &crate::session::turn_context::TurnContext) -> bool {
    turn.code_mode_available
        && crate::tools::effective_tool_mode(turn)
            != codex_protocol::openai_models::ToolMode::Direct
}

async fn schedule_startup_prewarm_inner(
    session: Arc<Session>,
    _base_instructions: String,
    startup_transport: Option<SessionStartupTransportHandle>,
) -> CodexResult<Option<ModelClientSession>> {
    let _preparation = session
        .startup_timing
        .begin_phase(StartupPhase::PrewarmPreparation);
    let prewarm_started_at = Instant::now();
    let startup_turn_context = session
        .new_startup_prewarm_turn_with_sub_id(INITIAL_SUBMIT_ID.to_owned())
        .await;
    // Reuse the exact resolved model used for startup planning; feature flags
    // alone do not determine whether the model exposes code-mode execution.
    session.schedule_code_mode_prewarm(startup_turn_context.as_ref());
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_create_turn_context",
        prewarm_started_at.elapsed(),
        /*status*/ None,
    );

    let startup_cancellation_token = CancellationToken::new();
    let tools_preparation = session.startup_timing.begin_tool_preparation();
    let mut planning_generation = session.services.planning_generation();
    let built_tools_started_at = Instant::now();
    // Startup prewarm runs before run_turn and needs its own tool-building snapshot.
    let mut step_context = session
        .capture_step_context(Arc::clone(&startup_turn_context))
        .await?;
    let captured_generation = session.services.planning_generation();
    if captured_generation != planning_generation {
        // The first capture may publish the MCP runtime's selected scope,
        // advancing the generation. Like pending-turn planning, recapture once
        // so the router is built from a snapshot taken wholly within the
        // generation it is keyed to.
        planning_generation = captured_generation;
        step_context = session
            .capture_step_context(Arc::clone(&startup_turn_context))
            .await?;
    }
    let startup_router = built_tools(
        session.as_ref(),
        &step_context,
        &[],
        &startup_cancellation_token,
    )
    .await?;
    startup_turn_context.session_telemetry.record_startup_phase(
        "startup_prewarm_build_tools",
        built_tools_started_at.elapsed(),
        /*status*/ None,
    );
    if session.services.planning_generation() == planning_generation {
        session
            .startup_prepared_router
            .publish(PreparedStartupRouter {
                planning_generation,
                config: Arc::clone(&startup_turn_context.config),
                dynamic_tools: startup_turn_context.dynamic_tools.clone(),
                router: Arc::clone(&startup_router),
            })
            .await;
    }
    drop(tools_preparation);
    // Connection ownership is resolved only after the preparation scope above ends.
    // Tool preparation is useful for every transport; only preconnect depends on websocket support.
    if !session.services.model_client.startup_websocket_enabled() {
        return Ok(None);
    }
    // Startup is handshake-only. An optional generate=false request would
    // hold a ready socket until its response drained and force first-turn
    // handoff to discard it. Never transfer a socket with in-flight output.
    Ok(resolve_startup_transport(startup_transport).await)
}
