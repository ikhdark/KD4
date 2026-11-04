use std::sync::Arc;

use super::config_errors::ConfigLoadResultExt;
use crate::config_manager::ConfigManager;
use crate::config_manager_service::ConfigManagerError;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol_types::ClientResponsePayload;
use codex_app_server_protocol_types::ConfigBatchWriteParams;
use codex_app_server_protocol_types::ConfigReadParams;
use codex_app_server_protocol_types::ConfigReadResponse;
use codex_app_server_protocol_types::ConfigRequirementsReadResponse;
use codex_app_server_protocol_types::ConfigValueWriteParams;
use codex_app_server_protocol_types::ConfigWriteErrorCode;
use codex_app_server_protocol_types::ConfigWriteResponse;
use codex_app_server_protocol_types::ExperimentalFeatureEnablementSetParams;
use codex_app_server_protocol_types::ExperimentalFeatureEnablementSetResponse;
use codex_app_server_protocol_types::GatewayOAuthReadResponse;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::ModelProviderCapabilitiesReadResponse;
use codex_core::ThreadManager;
use codex_features::Feature;
use codex_features::feature_for_key;
use codex_features::user_settable_feature_for_key;
use codex_model_provider::create_model_provider;
use codex_plugin::PluginId;
use futures::StreamExt;
use serde_json::json;
use std::path::PathBuf;

const SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT: &[&str] =
    &["auth_elicitation", "remote_plugin", "tool_suggest"];

fn runtime_refreshable_features_for_batch(params: &ConfigBatchWriteParams) -> Vec<Feature> {
    let paths = params
        .edits
        .iter()
        .filter_map(|edit| crate::config_manager_service::parse_key_path(&edit.key_path).ok())
        .collect::<Vec<_>>();
    let refresh_all = paths.iter().any(|path| path.as_slice() == ["features"]);
    SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT
        .iter()
        .filter(|feature_key| {
            refresh_all
                || paths
                    .iter()
                    .any(|path| path.as_slice() == ["features", **feature_key])
        })
        .filter_map(|feature_key| user_settable_feature_for_key(feature_key))
        .collect()
}

#[derive(Clone)]
pub(crate) struct ConfigRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    thread_manager: Arc<ThreadManager>,
    analytics_events_client: AnalyticsEventsClient,
}

impl ConfigRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
        analytics_events_client: AnalyticsEventsClient,
    ) -> Self {
        Self {
            outgoing,
            config_manager,
            thread_manager,
            analytics_events_client,
        }
    }

    pub(crate) async fn read(
        &self,
        params: ConfigReadParams,
    ) -> Result<ConfigReadResponse, JSONRPCErrorError> {
        let (mut response, features) = self.config_manager.read(params).await.map_err(map_error)?;
        for feature_key in SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT {
            let Some(feature) = feature_for_key(feature_key) else {
                continue;
            };
            let features_value = response
                .config
                .additional
                .entry("features".to_string())
                .or_insert_with(|| json!({}));
            if !features_value.is_object() {
                *features_value = json!({});
            }
            if let Some(features_object) = features_value.as_object_mut() {
                features_object
                    .insert((*feature_key).to_string(), json!(features.enabled(feature)));
            }
        }
        Ok(response)
    }

    pub(crate) async fn config_requirements_read(
        &self,
    ) -> Result<ConfigRequirementsReadResponse, JSONRPCErrorError> {
        let requirements = self
            .config_manager
            .read_requirements()
            .await
            .map_err(map_error)?
            .map(codex_app_server_config::config_requirements_to_api);

        Ok(ConfigRequirementsReadResponse { requirements })
    }

    pub(crate) async fn value_write(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        self.write_value(params)
            .await
            .map(ClientResponsePayload::ConfigValueWrite)
    }

    pub(crate) async fn batch_write(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        self.batch_write_inner(params)
            .await
            .map(ClientResponsePayload::ConfigBatchWrite)
    }

    pub(crate) async fn experimental_feature_enablement_set(
        &self,
        request_id: ConnectionRequestId,
        params: ExperimentalFeatureEnablementSetParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self.set_experimental_feature_enablement(params).await?;
        self.outgoing
            .send_response_as(
                request_id,
                ClientResponsePayload::ExperimentalFeatureEnablementSet(response),
            )
            .await;
        Ok(None)
    }

    pub(crate) async fn model_provider_capabilities_read(
        &self,
    ) -> Result<ModelProviderCapabilitiesReadResponse, JSONRPCErrorError> {
        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        let provider = create_model_provider(config.model_provider, /*auth_manager*/ None);
        let capabilities = provider.capabilities();
        Ok(ModelProviderCapabilitiesReadResponse {
            namespace_tools: capabilities.namespace_tools,
            image_generation: capabilities.image_generation,
            web_search: capabilities.web_search,
        })
    }

    /// This fork has no gateway-OAuth providers, so the effective provider never requires it.
    pub(crate) async fn gateway_oauth_read(
        &self,
    ) -> Result<GatewayOAuthReadResponse, JSONRPCErrorError> {
        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        Ok(GatewayOAuthReadResponse {
            provider_id: config.model_provider_id,
            provider_name: config.model_provider.name,
            required: false,
            status: None,
            error: None,
        })
    }

    pub(crate) fn handle_config_mutation(&self) {
        // Mutations may bypass ConfigManager writes; publish them to the next read.
        self.config_manager.invalidate_load_cache();
        self.thread_manager.plugins_manager().clear_cache();
        self.thread_manager.skills_service().clear_cache();
    }

    async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> Result<codex_core::config::Config, JSONRPCErrorError> {
        self.config_manager
            .load_latest_config(fallback_cwd)
            .await
            .map_config_load_error()
    }

    async fn write_value(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ConfigWriteResponse, JSONRPCErrorError> {
        let pending_changes = codex_core_plugins::toggles::collect_plugin_enabled_candidates_from_segments(
            crate::config_manager_service::parse_key_path(&params.key_path)
                .ok().into_iter().map(|segments| (segments, &params.value)),
        );
        let response = self
            .config_manager
            .write_value(params)
            .await
            .map_err(map_error)?;
        self.handle_config_mutation();
        self.emit_plugin_toggle_events(pending_changes).await;
        Ok(response)
    }

    async fn batch_write_inner(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ConfigWriteResponse, JSONRPCErrorError> {
        let reload_user_config = params.reload_user_config;
        let refreshed_features = runtime_refreshable_features_for_batch(&params);
        let pending_changes = codex_core_plugins::toggles::collect_plugin_enabled_candidates_from_segments(
            params
                .edits
                .iter()
                .filter_map(|edit| crate::config_manager_service::parse_key_path(&edit.key_path)
                    .ok().map(|segments| (segments, &edit.value))),
        );
        let response = self
            .config_manager
            .batch_write(params)
            .await
            .map_err(map_error)?;
        self.handle_config_mutation();
        self.emit_plugin_toggle_events(pending_changes).await;
        if reload_user_config {
            self.reload_user_config(&refreshed_features).await?;
        }
        Ok(response)
    }

    async fn set_experimental_feature_enablement(
        &self,
        params: ExperimentalFeatureEnablementSetParams,
    ) -> Result<ExperimentalFeatureEnablementSetResponse, JSONRPCErrorError> {
        let ExperimentalFeatureEnablementSetParams { mut enablement } = params;
        let mut invalid_keys = Vec::new();
        enablement.retain(|key, _| {
            let valid = user_settable_feature_for_key(key).is_some()
                && SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT.contains(&key.as_str());
            if !valid {
                invalid_keys.push(key.clone());
            }
            valid
        });
        if !invalid_keys.is_empty() {
            let invalid_keys = invalid_keys.join(", ");
            tracing::warn!("ignoring invalid experimental feature enablement keys: {invalid_keys}");
        }

        if enablement.is_empty() {
            return Ok(ExperimentalFeatureEnablementSetResponse { enablement });
        }
        let refreshed_features = enablement
            .keys()
            .filter_map(|name| user_settable_feature_for_key(name))
            .collect::<Vec<_>>();

        // Resolve the fallible base configuration before publishing overrides.
        // The owner rechecks this generation under its publication lock.
        let generation = self.config_manager.policy_generation();
        self.load_latest_config(/*fallback_cwd*/ None).await?;
        self.config_manager
            .extend_runtime_feature_enablement_for_generation(
                generation,
                enablement
                    .iter()
                    .map(|(name, enabled)| (name.clone(), *enabled)),
            )
            .map_config_load_error()?;
        self.handle_config_mutation();

        self.reload_user_config(&refreshed_features).await?;

        Ok(ExperimentalFeatureEnablementSetResponse { enablement })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The runtime-refresh owner spans per-thread config capture and publication"
    )]
    async fn reload_user_config(&self, refreshed_features: &[Feature]) -> Result<(), JSONRPCErrorError> {
        let _refresh_guard = self.config_manager.lock_runtime_refresh().await;
        let thread_ids = self.thread_manager.list_thread_ids().await;
        // handle_config_mutation already invalidated the shared discovery state.
        // Do not let one refreshed thread evict another's newly loaded plugins.
        let failures = futures::stream::iter(thread_ids)
            .map(|thread_id| async move {
                let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                    return None;
                };
                let current_config = thread.config().await;
                let next_config = match self
                    .config_manager
                    .load_latest_config_for_thread(current_config.as_ref())
                    .await
                {
                    Ok(config) => config,
                    Err(err) => {
                        tracing::warn!(
                            "failed to rebuild thread config for runtime refresh: {err}"
                        );
                        return Some((thread_id.to_string(), err.to_string()));
                    }
                };
                thread
                    .refresh_runtime_config_features_after_discovery_invalidation(
                        next_config,
                        refreshed_features,
                    )
                    .await;
                None
            })
            .buffer_unordered(5)
            .filter_map(|failure| async move { failure })
            .collect::<Vec<_>>()
            .await;
        if !failures.is_empty() {
            let mut error = internal_error("configuration committed, but some loaded threads could not be refreshed; retry the refresh");
            error.data = Some(json!({
                "reason": "runtimeRefreshIncomplete",
                "configurationCommitted": true,
                "failedThreads": failures.into_iter().map(|(thread_id, message)| json!({"threadId": thread_id, "message": message})).collect::<Vec<_>>(),
            }));
            return Err(error);
        }
        Ok(())
    }

    async fn emit_plugin_toggle_events(
        &self,
        pending_changes: std::collections::BTreeMap<String, bool>,
    ) {
        let plugins_manager = self.thread_manager.plugins_manager();
        let metadata = futures::stream::iter(pending_changes)
            .filter_map(|(plugin_id, enabled)| async move {
                PluginId::parse(&plugin_id).ok().map(|id| (id, enabled))
            })
            .map(|(plugin_id, enabled)| {
                let plugins_manager = &plugins_manager;
                async move {
                    let metadata = plugins_manager
                        .telemetry_metadata_for_installed_plugin(&plugin_id)
                        .await;
                    (metadata, enabled)
                }
            })
            .buffered(5);
        futures::pin_mut!(metadata);
        while let Some((metadata, enabled)) = metadata.next().await {
            if enabled {
                self.analytics_events_client.track_plugin_enabled(metadata);
            } else {
                self.analytics_events_client.track_plugin_disabled(metadata);
            }
        }
    }
}

fn map_error(err: ConfigManagerError) -> JSONRPCErrorError {
    if let Some(code) = err.write_error_code() {
        return config_write_error(code, err.to_string());
    }

    internal_error(err.to_string())
}

fn config_write_error(code: ConfigWriteErrorCode, message: impl Into<String>) -> JSONRPCErrorError {
    let mut error = invalid_request(message);
    error.data = Some(json!({
        "config_write_error_code": code,
    }));
    error
}

