//! Regression checks and opt-in measurements for the four skills recovery fixes.
//! These exercise subsystem behavior, not model-driven task completion or prompt caching.
#![cfg(windows)]
#![cfg(test)]

use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_core_skills::HostSkillsSnapshot;
use codex_core_skills::SkillLoadOutcome;
use codex_core_skills::SkillMetadata;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolPayload;
use codex_extension_api::WorldStateContributionInput;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SkillScope;
use codex_protocol::protocol::TruncationPolicy;
use codex_skills_extension::ExecutorSkillProvider;
use codex_skills_extension::SkillProviders;
use codex_skills_extension::SkillsExtensionConfig;
use codex_skills_extension::catalog;
use codex_skills_extension::catalog::SkillCatalog;
use codex_skills_extension::catalog::SkillReadResult;
use codex_skills_extension::install_with_providers;
use codex_skills_extension::provider;
use codex_skills_extension::provider::SkillListQuery;
use codex_skills_extension::provider::SkillProvider;
use codex_skills_extension::provider::SkillProviderFuture;
use codex_skills_extension::provider::SkillReadRequest;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;

// Compile the production pagination algorithm verbatim. Only its page transport is supplied
// by this fixture; the extension registry, state/cache, rendering, and public tools are real.
#[allow(dead_code)]
mod production_discovery {
    pub(super) async fn run(
        continuation: Option<crate::catalog::SkillDiscoveryContinuation>,
        timeout: Duration,
        calls: &std::sync::atomic::AtomicUsize,
        delay: Duration,
        repeat_cursor: bool,
    ) -> SkillCatalog {
        discover(continuation, timeout, |cursor| async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let index = usize::from(cursor.is_some());
            Ok::<_, ()>(codex_mcp::McpResourcePage {
                resources: vec![
                    serde_json::from_value(serde_json::json!({
                        "uri": format!("skill://probe/skill-{index}"),
                        "name": format!("skill-{index}"),
                        "mimeType": "mcp/skill",
                        "description": "Diagnostic fixture.",
                        "_meta": {"skill_name": format!("skill-{index}"), "source": "user"}
                    }))
                    .unwrap(),
                ],
                next_cursor: if repeat_cursor || (delay.is_zero() && cursor.is_none()) {
                    Some("same".to_string())
                } else {
                    None
                },
            })
        })
        .await
    }

    include!("../src/provider/orchestrator.rs");
}

const BODY: &str =
    "---\nname: probe\ndescription: Diagnostic fixture.\n---\nREQUIRED_PROBE_INSTRUCTION\n";
const RUNS: usize = 10;

struct FixtureRoot(PathBuf);

impl FixtureRoot {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codex-skills-probe-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("probe")).unwrap();
        fs::write(path.join("probe/SKILL.md"), BODY).unwrap();
        Self(path)
    }

    fn skill_path(&self) -> AbsolutePathBuf {
        AbsolutePathBuf::try_from(self.0.join("probe/SKILL.md")).unwrap()
    }

    fn selected(&self) -> SelectedCapabilityRoot {
        SelectedCapabilityRoot {
            id: "probe-root".to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: "local".to_string(),
                path: PathUri::from_host_native_path(&self.0).unwrap(),
            },
        }
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        // This guard owns only the unique directory successfully created by new().
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct CountingExecutor {
    inner: ExecutorSkillProvider,
    lists: AtomicUsize,
}

impl CountingExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: ExecutorSkillProvider::new_with_restriction_product(
                Arc::new(EnvironmentManager::default_for_tests()),
                None,
            ),
            lists: AtomicUsize::new(0),
        })
    }
}

impl SkillProvider for CountingExecutor {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(query)
    }

    fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult> {
        self.inner.read(request)
    }
}

struct PageProvider {
    calls: AtomicUsize,
    delay: Duration,
    repeat_cursor: bool,
}

impl SkillProvider for PageProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            Ok(production_discovery::run(
                query.continuation,
                query.discovery_timeout,
                &self.calls,
                self.delay,
                self.repeat_cursor,
            )
            .await)
        })
    }

    fn read(&self, _: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult> {
        Box::pin(async { panic!("catalog probes must not read selected instructions") })
    }
}

struct Harness {
    registry: ExtensionRegistry<()>,
    session: ExtensionData,
    thread: ExtensionData,
}

impl Harness {
    async fn new(providers: SkillProviders) -> Self {
        let mut builder = ExtensionRegistryBuilder::new();
        install_with_providers(&mut builder, providers, |_| SkillsExtensionConfig {
            include_instructions: true,
            bundled_skills_enabled: true,
            orchestrator_skills_enabled: true,
        });
        let harness = Self {
            registry: builder.build(),
            session: ExtensionData::new("session"),
            thread: ExtensionData::new("thread"),
        };
        harness.registry.thread_lifecycle_contributors()[0]
            .on_thread_start(ThreadStartInput {
                config: &(),
                session_source: &SessionSource::Cli,
                persistent_thread_state_available: true,
                environments: &[],
                session_store: &harness.session,
                thread_store: &harness.thread,
            })
            .await;
        harness
    }

    async fn executor_body(&self, root: &SelectedCapabilityRoot, turn_id: &str) -> Option<String> {
        let turn = ExtensionData::new(turn_id);
        let sections = self.registry.context_contributors()[0]
            .contribute_world_state(WorldStateContributionInput {
                thread_id: codex_protocol::ThreadId::new(),
                turn_id,
                environments: &[],
                ready_selected_capability_roots: std::slice::from_ref(root),
                session_store: &self.session,
                thread_store: &self.thread,
                turn_store: &turn,
            })
            .await;
        assert_eq!(sections.len(), 1);
        sections[0]
            .render_diff(PreviousWorldStateSection::Absent)
            .map(|fragment| fragment.body().to_string())
    }

    async fn list(&self, cursor: Option<&str>) -> Value {
        let tools = self.registry.tool_contributors()[0].tools(&self.session, &self.thread);
        let tool = tools
            .iter()
            .find(|tool| tool.tool_name().name == "list")
            .unwrap();
        let payload = ToolPayload::Function {
            arguments: json!({"authority":{"kind":"orchestrator"}, "cursor":cursor}).to_string(),
        };
        let call = ToolCall {
            turn_id: "turn".into(),
            call_id: "probe".into(),
            tool_name: tool.tool_name(),
            model: "no-model-used".into(),
            truncation_policy: TruncationPolicy::Bytes(8_000),
            source: codex_tools::ToolCallSource::Direct,
            conversation_history: Default::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        };
        tool.handle(call).await.unwrap().code_mode_result(&payload)
    }
}

fn summary(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    };
    json!({"n":sorted.len(), "min_ms":sorted[0], "median_ms":median, "max_ms":sorted[sorted.len()-1]})
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

async fn locator_probe() -> Value {
    let root = FixtureRoot::new();
    let executor = CountingExecutor::new();
    let harness =
        Harness::new(SkillProviders::new().with_executor_provider(executor.clone())).await;
    let rendered = harness
        .executor_body(&root.selected(), "one")
        .await
        .unwrap();
    let arguments: Value = serde_json::from_str(
        rendered
            .lines()
            .find_map(|line| {
                line.split_once("(read_file: ")
                    .map(|(_, args)| args.strip_suffix(')').unwrap())
            })
            .expect("the catalog must advertise an executable read_file route"),
    )
    .unwrap();
    assert_eq!(arguments["environment_id"], "local");
    assert!(!rendered.contains("skill://"));
    let path = root.skill_path();
    let mut outcome = SkillLoadOutcome::default();
    outcome.skills.push(SkillMetadata {
        name: "probe".into(),
        description: "Diagnostic fixture.".into(),
        short_description: None,
        interface: None,
        dependencies: None,
        policy: None,
        path_to_skills_md: path.clone(),
        scope: SkillScope::User,
        plugin_id: None,
    });
    let host = HostSkillsSnapshot::new(Arc::new(outcome));
    // Opaque identities remain valid for explicit mentions, but the advertised read route
    // must bypass the host-only skill: resolver and use the owning environment filesystem.
    let locator = format!(
        "skill://probe-root/{}",
        path.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    assert!(
        host.read_skill_text_by_catalog_locator(&locator)
            .await
            .is_err()
    );
    let manager = EnvironmentManager::default_for_tests();
    let environment = manager
        .get_environment(arguments["environment_id"].as_str().unwrap())
        .unwrap();
    let advertised_path =
        PathUri::from_host_native_path(arguments["path"].as_str().unwrap()).unwrap();
    assert_eq!(advertised_path, PathUri::from_abs_path(&path));
    let mut reads = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let contents = environment
            .get_filesystem()
            .read_file_text(&advertised_path, None)
            .await
            .unwrap();
        reads.push(elapsed_ms(start));
        assert_eq!(contents, BODY);
    }
    json!({"advertised_route_successes":100, "environment_file_read":summary(&reads),
        "scope":"production catalog arguments and owning environment filesystem, not the complete read_file handler"})
}

async fn failed_scan_probe() -> Value {
    let mut denied = Vec::new();
    let mut cached = Vec::new();
    let mut recovered = Vec::new();
    for _ in 0..RUNS {
        let root = FixtureRoot::new();
        fs::create_dir(root.0.join("healthy")).unwrap();
        fs::write(
            root.0.join("healthy/SKILL.md"),
            BODY.replace("name: probe", "name: healthy"),
        )
        .unwrap();
        let executor = CountingExecutor::new();
        let providers = SkillProviders::new().with_executor_provider(executor.clone());
        let harness = Harness::new(providers.clone()).await;
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.skill_path())
            .unwrap();
        assert!(fs::read_to_string(root.skill_path()).is_err());
        let start = Instant::now();
        let partial = harness
            .executor_body(&root.selected(), "first")
            .await
            .unwrap();
        assert!(partial.contains("- healthy:"));
        assert!(!partial.contains("- probe:"));
        denied.push(elapsed_ms(start));
        assert_eq!(executor.lists.load(Ordering::SeqCst), 1);
        drop(lock);
        assert_eq!(fs::read_to_string(root.skill_path()).unwrap(), BODY);
        assert_eq!(
            harness
                .executor_body(&root.selected(), "first")
                .await
                .as_deref(),
            Some(partial.as_str())
        );
        assert_eq!(
            executor.lists.load(Ordering::SeqCst),
            1,
            "no retry within the same turn"
        );
        let start = Instant::now();
        let restored = harness
            .executor_body(&root.selected(), "second")
            .await
            .unwrap();
        assert!(restored.contains("- probe:") && restored.contains("- healthy:"));
        cached.push(elapsed_ms(start));
        assert_eq!(executor.lists.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness
                .executor_body(&root.selected(), "third")
                .await
                .as_deref(),
            Some(restored.as_str())
        );
        assert_eq!(
            executor.lists.load(Ordering::SeqCst),
            2,
            "successful recovery remains cached"
        );
        let fresh = Harness::new(providers).await;
        let start = Instant::now();
        assert!(
            fresh
                .executor_body(&root.selected(), "third")
                .await
                .unwrap()
                .contains("- probe:")
        );
        recovered.push(elapsed_ms(start));
        assert_eq!(executor.lists.load(Ordering::SeqCst), 3);
    }
    json!({"runs":RUNS,"same_thread_recoveries":RUNS,"fresh_thread_recoveries":RUNS,
        "denied_scan":summary(&denied),"next_turn_recovery":summary(&cached),"fresh_scan":summary(&recovered)})
}

async fn timeout_probe() -> Value {
    let source = include_str!("../../../core/src/session/mod.rs");
    let declaration = source
        .lines()
        .find(|line| line.contains("const EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT:"))
        .unwrap();
    let seconds: u64 = declaration
        .split("Duration::from_secs(")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let host_timeout = Duration::from_secs(seconds);
    let provider = Arc::new(PageProvider {
        calls: AtomicUsize::new(0),
        delay: host_timeout + Duration::from_secs(1),
        repeat_cursor: false,
    });
    let harness =
        Harness::new(SkillProviders::new().with_orchestrator_provider(provider.clone())).await;
    let contributor = &harness.registry.context_contributors()[0];
    let mut automatic = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let fragments = tokio::time::timeout(
            host_timeout,
            contributor.contribute_thread_context(&harness.session, &harness.thread),
        )
        .await
        .expect("automatic discovery must publish before the host deadline");
        assert_eq!(fragments.len(), 1);
        assert!(fragments[0].text().contains("discovery is incomplete"));
        automatic.push(elapsed_ms(start));
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "cached incomplete discovery must not retry automatically"
    );
    let start = Instant::now();
    let explicit = harness.list(None).await;
    let explicit_ms = elapsed_ms(start);
    assert_eq!(explicit["skills"][0]["name"], "skill-0");
    assert!(explicit["next_cursor"].is_null());
    let start = Instant::now();
    let warm = contributor
        .contribute_thread_context(&harness.session, &harness.thread)
        .await;
    let warm_ms = elapsed_ms(start);
    assert_eq!(warm.len(), 1);
    assert!(warm[0].text().contains("skill-0"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    json!({"host_budget_ms":host_timeout.as_millis(),"fixture_response_delay_ms":provider.delay.as_millis(),
        "automatic_timeouts":0,"automatic_page_attempts":1,"automatic_wait":summary(&automatic),
        "automatic_wait_samples_ms":automatic,
        "explicit_existing_recovery_ms":explicit_ms,"warm_after_recovery_ms":warm_ms,
        "scope":"real-clock production discovery and extension cache under the host's source-derived timeout; simulated transport; no app-server/model"})
}

async fn cursor_probe() -> Value {
    let mut stuck_runs = Vec::new();
    let mut control_runs = Vec::new();
    let mut response_bytes = Vec::new();
    for _ in 0..RUNS {
        let provider = Arc::new(PageProvider {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            repeat_cursor: true,
        });
        let harness =
            Harness::new(SkillProviders::new().with_orchestrator_provider(provider.clone())).await;
        let start = Instant::now();
        let first = harness.list(None).await;
        assert_eq!(first["skills"].as_array().unwrap().len(), 2);
        assert!(first["next_cursor"].is_null());
        assert_eq!(first["discovery_blocked"], true);
        let mut bytes = serde_json::to_vec(&first).unwrap().len();
        for _ in 0..4 {
            let next = harness.list(None).await;
            assert_eq!(next["skills"].as_array().unwrap().len(), 2);
            assert!(next["next_cursor"].is_null());
            assert_eq!(next["discovery_blocked"], true);
            assert!(
                next["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|warning| warning.as_str().unwrap().contains("repeated a cursor"))
            );
            bytes += serde_json::to_vec(&next).unwrap().len();
        }
        stuck_runs.push(elapsed_ms(start));
        response_bytes.push(bytes);
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "blocked first-page requests must not hit the provider"
        );
        let provider = Arc::new(PageProvider {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            repeat_cursor: false,
        });
        let harness =
            Harness::new(SkillProviders::new().with_orchestrator_provider(provider.clone())).await;
        let start = Instant::now();
        let complete = harness.list(None).await;
        control_runs.push(elapsed_ms(start));
        assert_eq!(complete["skills"].as_array().unwrap().len(), 2);
        assert!(complete["next_cursor"].is_null());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }
    json!({"runs":RUNS,"blocked_tool_calls_per_run":5,"blocked_page_attempts_per_run":2,
        "advertised_nonadvancing_continuations":0,"distinct_skills_per_run":2,
        "valid_control_tool_calls_per_run":1,"valid_control_page_attempts_per_run":2,
        "stalled_run":summary(&stuck_runs),"valid_control_run":summary(&control_runs),
        "stalled_response_bytes_per_run":response_bytes})
}

async fn run_probes() {
    let start = Instant::now();
    let results = json!({
        "locator":locator_probe().await,
        "failed_scan":failed_scan_probe().await,
        "timeout":timeout_probe().await,
        "cursor":cursor_probe().await,
        "scope":"subsystem regression checks and timings; no app-server/model or prompt-cache measurement",
        "execution_ms":elapsed_ms(start),
    });
    println!(
        "SKILLS_REVIEW_PROBE {}",
        serde_json::to_string(&results).unwrap()
    );
    if let Some(path) = std::env::var_os("SKILLS_REVIEW_PROBE_OUTPUT") {
        fs::write(path, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn skills_recovery_contracts() {
    run_probes().await;
}

#[tokio::test]
#[ignore = "opt-in repeated measurements; skills_recovery_contracts covers the same assertions"]
async fn measure_review_findings() {
    run_probes().await;
}
