use crate::config_manager::ConfigManager;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::Op;
use futures::StreamExt;
use futures::stream;
use std::future::Future;
use std::io;
use std::sync::Arc;
use tracing::warn;

const MCP_REFRESH_CONCURRENCY: usize = 8;

struct PlannedRefresh {
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: McpServerRefreshConfig,
}

pub(crate) async fn queue_strict_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) -> io::Result<()> {
    config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await?;
    let refreshes = collect_strict_results(plan_refreshes(thread_manager, config_manager).await)?;
    collect_strict_results(queue_planned_refreshes(refreshes).await)?;
    Ok(())
}

pub(crate) async fn queue_best_effort_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) {
    let planned = plan_refreshes(thread_manager, config_manager).await;
    let mut refreshes = Vec::with_capacity(planned.len());
    for (thread_id, result) in planned {
        match result {
            Ok(refresh) => refreshes.push(refresh),
            Err(err) => warn!("failed to plan MCP refresh for thread {thread_id}: {err}"),
        }
    }
    for (_thread_id, result) in queue_planned_refreshes(refreshes).await {
        if let Err(err) = result {
            warn!("{err}");
        }
    }
}

async fn plan_refreshes(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) -> Vec<(ThreadId, io::Result<PlannedRefresh>)> {
    let mut thread_ids = thread_manager.list_thread_ids().await;
    thread_ids.sort_by_key(|thread_id| thread_id.to_string());
    let jobs = thread_ids.into_iter().map(|thread_id| async move {
        let result = async {
            let thread = thread_manager.get_thread(thread_id).await.map_err(|err| {
                io::Error::other(format!("failed to load thread {thread_id}: {err}"))
            })?;
            let config = build_refresh_config(thread.as_ref(), config_manager).await?;
            Ok(PlannedRefresh {
                thread_id,
                thread,
                config,
            })
        }
        .await;
        (thread_id, result)
    });
    let mut planned = collect_bounded(jobs).await;
    planned.sort_by_key(|(thread_id, _)| thread_id.to_string());
    planned
}

fn group_identical_refreshes<T>(
    refreshes: Vec<(McpServerRefreshConfig, T)>,
) -> Vec<(McpServerRefreshConfig, Vec<T>)> {
    let mut groups: Vec<(McpServerRefreshConfig, Vec<T>)> = Vec::new();
    for (config, refresh) in refreshes {
        if let Some(group) = groups.iter_mut().find_map(|(group_config, group)| {
            (&*group_config == &config).then_some(group)
        }) {
            group.push(refresh);
        } else {
            groups.push((config, vec![refresh]));
        }
    }
    groups
}

async fn queue_planned_refreshes(
    refreshes: Vec<PlannedRefresh>,
) -> Vec<(ThreadId, io::Result<()>)> {
    let mut submissions = Vec::with_capacity(refreshes.len());
    let keyed_refreshes = refreshes
        .into_iter()
        .map(|refresh| {
            (
                refresh.config,
                (refresh.thread_id, refresh.thread),
            )
        })
        .collect();
    for (config, threads) in group_identical_refreshes(keyed_refreshes) {
        for (thread_id, thread) in threads {
            submissions.push((thread_id, thread, config.clone()));
        }
    }
    let jobs = submissions
        .into_iter()
        .map(|(thread_id, thread, config)| async move {
            let result = queue_refresh(thread_id, thread, config).await;
            (thread_id, result)
        });
    let mut results = collect_bounded(jobs).await;
    results.sort_by_key(|(thread_id, _)| thread_id.to_string());
    results
}

async fn collect_bounded<I, F, T>(jobs: I) -> Vec<T>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = T>,
{
    stream::iter(jobs)
        .buffer_unordered(MCP_REFRESH_CONCURRENCY)
        .collect()
        .await
}

fn collect_strict_results<T>(
    mut results: Vec<(ThreadId, io::Result<T>)>,
) -> io::Result<Vec<T>> {
    results.sort_by_key(|(thread_id, _)| thread_id.to_string());
    results.into_iter().map(|(_thread_id, result)| result).collect()
}

async fn build_refresh_config(
    thread: &CodexThread,
    config_manager: &ConfigManager,
) -> io::Result<McpServerRefreshConfig> {
    let thread_config = thread.config().await;
    let config = config_manager
        .load_latest_config_for_thread(thread_config.as_ref())
        .await?;
    let mcp_config = thread.runtime_mcp_config(&config).await;
    let mcp_servers = codex_mcp::configured_mcp_servers(&mcp_config);
    Ok(McpServerRefreshConfig {
        mcp_servers: serde_json::to_value(mcp_servers).map_err(io::Error::other)?,
        mcp_oauth_credentials_store_mode: serde_json::to_value(
            config.mcp_oauth_credentials_store_mode,
        )
        .map_err(io::Error::other)?,
        auth_keyring_backend_kind: serde_json::to_value(config.auth_keyring_backend_kind())
            .map_err(io::Error::other)?,
    })
}

async fn queue_refresh(
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: McpServerRefreshConfig,
) -> io::Result<()> {
    thread
        .submit(Op::RefreshMcpServers { config })
        .await
        .map(|_| ())
        .map_err(|err| {
            io::Error::other(format!(
                "failed to queue MCP refresh for thread {thread_id}: {err}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::ThreadExtensionDependencies;
    use crate::extensions::guardian_agent_spawner;
    use crate::extensions::thread_extensions;
    use codex_arg0::Arg0DispatchPaths;
    use codex_config::CloudConfigBundleLoader;
    use codex_config::LoaderOverrides;
    use codex_config::ThreadConfigContext;
    use codex_config::ThreadConfigLoadError;
    use codex_config::ThreadConfigLoadErrorCode;
    use codex_config::ThreadConfigLoader;
    use codex_config::ThreadConfigSource;
    use codex_config::types::AuthKeyringBackendKind;
    use codex_core::config::ConfigOverrides;
    use codex_core::init_state_db;
    use codex_core::thread_store_from_config;
    use codex_exec_server::EnvironmentManager;
    use codex_extension_api::NoopExtensionEventSink;
    use codex_home::CodexHomeUserInstructionsProvider;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
    use codex_protocol::protocol::SessionSource;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::str::FromStr;
    use tempfile::TempDir;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn strict_refresh_reports_thread_planning_failures() -> anyhow::Result<()> {
        let (_temp_dir, thread_manager, config_manager, _loader) = refresh_test_state().await?;

        let err = queue_strict_refresh(&thread_manager, &config_manager)
            .await
            .expect_err("strict refresh should fail");

        assert_eq!(err.to_string(), "failed to load refresh config");
        Ok(())
    }

    #[tokio::test]
    async fn best_effort_refresh_attempts_every_loaded_thread() -> anyhow::Result<()> {
        let (_temp_dir, thread_manager, config_manager, loader) = refresh_test_state().await?;

        queue_best_effort_refresh(&thread_manager, &config_manager).await;

        assert_eq!(loader.good_loads.load(Ordering::Relaxed), 1);
        assert_eq!(loader.bad_loads.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_config_uses_latest_auth_keyring_backend() -> anyhow::Result<()> {
        let (temp_dir, thread_manager, config_manager, _loader) = refresh_test_state().await?;
        std::fs::write(
            temp_dir.path().join(codex_config::CONFIG_TOML_FILE),
            "[features]\nsecret_auth_storage = true\n",
        )?;

        let mut good_thread = None;
        for thread_id in thread_manager.list_thread_ids().await {
            let thread = thread_manager.get_thread(thread_id).await?;
            let thread_config = thread.config().await;
            if thread_config.cwd.ends_with("good") {
                good_thread = Some(thread);
                break;
            }
        }
        let thread = good_thread.expect("good test thread should exist");

        let refresh_config = build_refresh_config(thread.as_ref(), &config_manager).await?;
        let backend = serde_json::from_value::<AuthKeyringBackendKind>(
            refresh_config.auth_keyring_backend_kind,
        )?;

        assert_eq!(
            thread.config().await.auth_keyring_backend_kind(),
            AuthKeyringBackendKind::Direct
        );
        assert_eq!(backend, AuthKeyringBackendKind::Secrets);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_fanout_is_bounded_without_serializing_blocked_work() {
        let job_count = MCP_REFRESH_CONCURRENCY * 3;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let release_blocked = Arc::new(Notify::new());

        let jobs = (0..job_count).map(|job| {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let completed = Arc::clone(&completed);
            let release_blocked = Arc::clone(&release_blocked);
            async move {
                let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now_active, Ordering::SeqCst);
                if job == 0 {
                    release_blocked.notified().await;
                } else {
                    completed.fetch_add(1, Ordering::SeqCst);
                }
                active.fetch_sub(1, Ordering::SeqCst);
            }
        });
        let fanout = tokio::spawn(collect_bounded(jobs));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while completed.load(Ordering::SeqCst) != job_count - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one blocked refresh must not delay independent refresh work");
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= MCP_REFRESH_CONCURRENCY);

        release_blocked.notify_one();
        fanout.await.expect("bounded refresh fanout should finish");
    }

    #[test]
    fn strict_refresh_errors_are_reported_in_thread_order() {
        let first = ThreadId::from_str("11111111-1111-4111-8111-111111111111")
            .expect("valid first thread ID");
        let second = ThreadId::from_str("22222222-2222-4222-8222-222222222222")
            .expect("valid second thread ID");

        let err = collect_strict_results::<()>(vec![
            (second, Err(io::Error::other("second error"))),
            (first, Err(io::Error::other("first error"))),
        ])
        .expect_err("strict refresh should report an error");

        assert_eq!(err.to_string(), "first error");
    }

    #[test]
    fn identical_refresh_keys_share_one_deterministic_group() {
        let config_a = McpServerRefreshConfig {
            mcp_servers: serde_json::json!({"docs": {"command": "docs"}}),
            mcp_oauth_credentials_store_mode: serde_json::json!("auto"),
            auth_keyring_backend_kind: serde_json::json!("direct"),
        };
        let config_b = McpServerRefreshConfig {
            mcp_servers: serde_json::json!({"other": {"command": "other"}}),
            ..config_a.clone()
        };

        let groups = group_identical_refreshes(vec![
            (config_a.clone(), "first"),
            (config_b.clone(), "second"),
            (config_a.clone(), "third"),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], (config_a, vec!["first", "third"]));
        assert_eq!(groups[1], (config_b, vec!["second"]));
    }

    async fn refresh_test_state() -> anyhow::Result<(
        TempDir,
        Arc<ThreadManager>,
        ConfigManager,
        Arc<CountingThreadConfigLoader>,
    )> {
        let temp_dir = TempDir::new()?;
        let good_cwd = temp_dir.path().join("good");
        let bad_cwd = temp_dir.path().join("bad");
        std::fs::create_dir_all(&good_cwd)?;
        std::fs::create_dir_all(&bad_cwd)?;
        std::fs::write(
            temp_dir.path().join(codex_config::CONFIG_TOML_FILE),
            "[features]\nsecret_auth_storage = false\n",
        )?;

        let initial_config_manager =
            ConfigManager::without_managed_config_for_tests(temp_dir.path().to_path_buf());
        let good_config = initial_config_manager
            .load_for_cwd(
                /*request_overrides*/ None,
                ConfigOverrides::default(),
                Some(good_cwd.clone()),
            )
            .await?;
        let bad_config = initial_config_manager
            .load_for_cwd(
                /*request_overrides*/ None,
                ConfigOverrides::default(),
                Some(bad_cwd.clone()),
            )
            .await?;

        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let state_db = init_state_db(&good_config)
            .await
            .expect("refresh tests require state db");
        let thread_store = thread_store_from_config(&good_config, Some(state_db.clone()));
        let environment_manager = Arc::new(EnvironmentManager::default_for_tests());
        let executor_skill_provider: Arc<dyn codex_skills_extension::SkillProvider> = Arc::new(
            codex_skills_extension::ExecutorSkillProvider::new_with_restriction_product(
                Arc::clone(&environment_manager),
                SessionSource::Exec.restriction_product(),
            ),
        );
        let thread_manager = Arc::new_cyclic(|thread_manager| {
            ThreadManager::new(
                &good_config,
                auth_manager.clone(),
                SessionSource::Exec,
                Arc::clone(&environment_manager),
                thread_extensions(
                    guardian_agent_spawner(thread_manager.clone()),
                    ThreadExtensionDependencies {
                        event_sink: Arc::new(NoopExtensionEventSink),
                        auth_manager: auth_manager.clone(),
                        state_db: Some(state_db.clone()),
                        analytics_events_client: codex_analytics::AnalyticsEventsClient::disabled(),
                        thread_manager: thread_manager.clone(),
                        goal_service: Arc::new(codex_goal_extension::GoalService::new()),
                        environment_manager: Arc::clone(&environment_manager),
                        executor_skill_provider: Arc::clone(&executor_skill_provider),
                        thread_store: Arc::clone(&thread_store),
                    },
                ),
                Arc::new(CodexHomeUserInstructionsProvider::new(
                    good_config.codex_home.clone(),
                )),
                /*analytics_events_client*/ None,
                Arc::clone(&thread_store),
                codex_core::local_agent_graph_store_from_state_db(Some(&state_db)),
                "11111111-1111-4111-8111-111111111111".to_string(),
                /*attestation_provider*/ None,
                /*external_time_provider*/ None,
            )
        });
        thread_manager.start_thread(good_config).await?;
        thread_manager.start_thread(bad_config).await?;

        let loader = Arc::new(CountingThreadConfigLoader {
            good_cwd: AbsolutePathBuf::try_from(good_cwd)?,
            bad_cwd: AbsolutePathBuf::try_from(bad_cwd)?,
            good_loads: AtomicUsize::new(0),
            bad_loads: AtomicUsize::new(0),
        });
        let config_manager = ConfigManager::new(
            temp_dir.path().to_path_buf(),
            Vec::new(),
            LoaderOverrides::without_managed_config_for_tests(),
            /*strict_config*/ false,
            CloudConfigBundleLoader::default(),
            Arg0DispatchPaths::default(),
            loader.clone(),
        );

        Ok((temp_dir, thread_manager, config_manager, loader))
    }

    struct CountingThreadConfigLoader {
        good_cwd: AbsolutePathBuf,
        bad_cwd: AbsolutePathBuf,
        good_loads: AtomicUsize,
        bad_loads: AtomicUsize,
    }

    impl CountingThreadConfigLoader {
        async fn load(
            &self,
            context: ThreadConfigContext,
        ) -> Result<Vec<ThreadConfigSource>, ThreadConfigLoadError> {
            if context.cwd.as_ref() == Some(&self.good_cwd) {
                self.good_loads.fetch_add(1, Ordering::Relaxed);
            }
            if context.cwd.as_ref() == Some(&self.bad_cwd) {
                self.bad_loads.fetch_add(1, Ordering::Relaxed);
                return Err(ThreadConfigLoadError::new(
                    ThreadConfigLoadErrorCode::Internal,
                    /*status_code*/ None,
                    "failed to load refresh config",
                ));
            }
            Ok(Vec::new())
        }
    }

    impl ThreadConfigLoader for CountingThreadConfigLoader {
        fn load(
            &self,
            context: ThreadConfigContext,
        ) -> codex_config::ThreadConfigLoaderFuture<'_, Vec<ThreadConfigSource>> {
            Box::pin(CountingThreadConfigLoader::load(self, context))
        }
    }
}
