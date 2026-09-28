//! Opt-in measurements and correctness probes for the core-skills efficiency review.
//! Run this integration target with `--ignored --nocapture --test-threads=1`.
//! The cache probes assert the desired contract, so they fail while the reviewed bugs exist.
#![cfg(test)]

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_core_skills::HostSkillsSnapshot;
use codex_core_skills::PluginSkillSnapshots;
use codex_core_skills::SkillsLoadInput;
use codex_core_skills::SkillsService;
use codex_core_skills::loader::SkillRoot;
use codex_core_skills::loader::load_skills_from_roots;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_exec_server::WalkOptions;
use codex_exec_server::WalkOutcome;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tempfile::TempDir;
use tokio::sync::Notify;

const PLUGINS: usize = 8;
const SKILLS_PER_PLUGIN: usize = 4;
const CACHE_ATTEMPTS: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Calls {
    walks: usize,
    canonicalizes: usize,
    reads: usize,
    bytes: usize,
    metadata: usize,
    manifest_metadata: usize,
}

#[derive(Default)]
struct ReadGate {
    started: Notify,
    release: Notify,
}

#[derive(Default)]
struct ProbeFs {
    calls: Mutex<Calls>,
    read_gate: Mutex<Option<Arc<ReadGate>>>,
    manifest_delay: Duration,
}

impl ProbeFs {
    fn calls(&self) -> Calls {
        *self.calls.lock().unwrap()
    }
}

impl ExecutorFileSystem for ProbeFs {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        self.calls.lock().unwrap().canonicalizes += 1;
        LOCAL_FS.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.calls.lock().unwrap().reads += 1;
            let bytes = LOCAL_FS.read_file(path, sandbox).await?;
            self.calls.lock().unwrap().bytes += bytes.len();
            let gate = if path.basename().as_deref() == Some("SKILL.md") {
                self.read_gate.lock().unwrap().take()
            } else {
                None
            };
            if let Some(gate) = gate {
                // Capture real file bytes before the other future edits and invalidates them.
                gate.started.notify_one();
                gate.release.notified().await;
            }
            Ok(bytes)
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        LOCAL_FS.read_file_stream(path, sandbox)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.write_file(path, contents, sandbox)
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            let manifest = path.basename().as_deref() == Some("plugin.json");
            {
                let mut calls = self.calls.lock().unwrap();
                calls.metadata += 1;
                calls.manifest_metadata += usize::from(manifest);
            }
            if manifest && !self.manifest_delay.is_zero() {
                tokio::time::sleep(self.manifest_delay).await;
            }
            LOCAL_FS.get_metadata(path, sandbox).await
        })
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        LOCAL_FS.read_directory(path, sandbox)
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        self.calls.lock().unwrap().walks += 1;
        LOCAL_FS.walk(path, options, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.copy(source_path, destination_path, options, sandbox)
    }
}

fn absolute(path: impl Into<PathBuf>) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(path.into()).unwrap()
}

fn report(label: &str, samples: &[Duration], calls: Calls) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2].as_secs_f64() * 1_000.0;
    let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1].as_secs_f64() * 1_000.0;
    let raw_us = samples.iter().map(Duration::as_micros).collect::<Vec<_>>();
    println!(
        "SKILLS_PROBE {{\"case\":\"{label}\",\"samples\":{},\"median_ms\":{median:.3},\"p95_ms\":{p95:.3},\"walks\":{},\"canonicalizes\":{},\"reads\":{},\"bytes\":{},\"metadata\":{},\"manifest_metadata\":{},\"raw_us\":{raw_us:?}}}",
        samples.len(),
        calls.walks,
        calls.canonicalizes,
        calls.reads,
        calls.bytes,
        calls.metadata,
        calls.manifest_metadata,
    );
}

fn plugin_roots(root: &Path, file_system: Arc<dyn ExecutorFileSystem>) -> Vec<SkillRoot> {
    (0..PLUGINS)
        .map(|index| {
            let plugin_root = absolute(root.join(format!("plugin-{index}")));
            SkillRoot {
                path: plugin_root.join("skills"),
                scope: SkillScope::User,
                file_system: Arc::clone(&file_system),
                plugin_id: Some(format!("plugin-{index}@fixture")),
                plugin_namespace: Some(format!("plugin-{index}")),
                plugin_root: Some(plugin_root),
            }
        })
        .collect()
}

#[tokio::test]
#[ignore = "opt-in real-filesystem microbenchmark"]
async fn measure_plugin_preload() {
    let fixture = tempfile::tempdir().unwrap();
    for plugin in 0..PLUGINS {
        for skill in 0..SKILLS_PER_PLUGIN {
            let directory = fixture
                .path()
                .join(format!("plugin-{plugin}/skills/skill-{skill}"));
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("SKILL.md"),
                format!(
                    "---\nname: skill-{skill}\ndescription: fixture {plugin}/{skill}\n---\n{}",
                    "x".repeat(8_192)
                ),
            )
            .unwrap();
        }
    }
    let mut samples = [Vec::new(), Vec::new()];
    let mut final_calls = [Calls::default(); 2];
    for iteration in 0..33 {
        // Alternate order to avoid always giving the preload case the warmer OS file cache.
        for index in if iteration % 2 == 0 { [0, 1] } else { [1, 0] } {
            let file_system = Arc::new(ProbeFs::default());
            let snapshots = (index == 1).then(PluginSkillSnapshots::for_plugin_load);
            let started = Instant::now();
            let first = load_skills_from_roots(
                plugin_roots(fixture.path(), file_system.clone()),
                snapshots.as_ref(),
            )
            .await;
            let first_calls = file_system.calls();
            let second = load_skills_from_roots(
                plugin_roots(fixture.path(), file_system.clone()),
                snapshots.as_ref(),
            )
            .await;
            let elapsed = started.elapsed();
            let calls = file_system.calls();
            assert!(first.errors.is_empty() && second.errors.is_empty());
            assert_eq!(first.skills.len(), PLUGINS * SKILLS_PER_PLUGIN);
            assert_eq!(second.skills, first.skills);
            assert_eq!(
                calls.reads,
                PLUGINS * SKILLS_PER_PLUGIN * if index == 0 { 2 } else { 1 }
            );
            assert_eq!(calls.walks, PLUGINS * if index == 0 { 2 } else { 1 });
            if index == 1 {
                assert_eq!(
                    calls, first_calls,
                    "preload reuse must perform no second-pass filesystem operations"
                );
            }
            if iteration >= 3 {
                samples[index].push(elapsed);
            }
            final_calls[index] = calls;
        }
    }
    report(
        "plugin_two_passes_without_preload",
        &samples[0],
        final_calls[0],
    );
    report(
        "plugin_two_passes_with_preload",
        &samples[1],
        final_calls[1],
    );
}

#[tokio::test]
#[ignore = "opt-in real-filesystem microbenchmark; candidate flag checks a separately patched loader"]
async fn measure_empty_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = (0..4)
        .map(|index| absolute(fixture.path().join(format!("branch-{index}/nested/skills"))))
        .collect::<Vec<_>>();
    for path in &paths[..2] {
        fs::create_dir_all(path).unwrap();
    }
    let candidate = std::env::var_os("SKILLS_EMPTY_ROOT_CANDIDATE").is_some();
    for delay_ms in [0, 5] {
        let sample_count = if delay_ms == 0 { 30 } else { 10 };
        let mut samples = Vec::new();
        let mut final_calls = Calls::default();
        for iteration in 0..sample_count + 3 {
            let file_system = Arc::new(ProbeFs {
                manifest_delay: Duration::from_millis(delay_ms),
                ..Default::default()
            });
            let roots = paths
                .iter()
                .map(|path| SkillRoot {
                    path: path.clone(),
                    scope: SkillScope::User,
                    file_system: file_system.clone(),
                    plugin_id: None,
                    plugin_namespace: None,
                    plugin_root: None,
                })
                .collect::<Vec<_>>();
            let started = Instant::now();
            let outcome = load_skills_from_roots(roots, None).await;
            let elapsed = started.elapsed();
            let calls = file_system.calls();
            assert!(outcome.skills.is_empty());
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            assert_eq!(calls.walks, 4);
            assert_eq!(calls.canonicalizes, 4);
            if candidate {
                assert_eq!(
                    calls.manifest_metadata, 0,
                    "empty roots have no skills to namespace"
                );
            } else {
                assert!(
                    calls.manifest_metadata > 0,
                    "baseline must exercise the reviewed namespace path"
                );
            }
            if iteration >= 3 {
                samples.push(elapsed);
            }
            final_calls = calls;
        }
        report(
            &format!("empty_roots_delay_{delay_ms}ms_candidate_{candidate}"),
            &samples,
            final_calls,
        );
    }
}

struct ServiceFixture {
    _directory: TempDir,
    skill_path: PathBuf,
    input: SkillsLoadInput,
    service: SkillsService,
    file_system: Arc<ProbeFs>,
}

impl ServiceFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(".git"), "gitdir: fixture\n").unwrap();
        let dot_codex = absolute(directory.path().join(".codex"));
        let skill_path = dot_codex.join("skills/demo/SKILL.md").to_path_buf();
        fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new(
                ConfigLayerSource::Project {
                    dot_codex_folder: dot_codex,
                },
                toml::Value::Table(toml::map::Map::new()),
            )],
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let input = SkillsLoadInput::new(absolute(directory.path()), Vec::new(), stack, false);
        let service = SkillsService::new(absolute(directory.path().join("home")), false);
        let fixture = Self {
            _directory: directory,
            skill_path,
            input,
            service,
            file_system: Arc::new(ProbeFs::default()),
        };
        fixture.write("before");
        fixture
    }

    fn write(&self, description: &str) {
        fs::write(
            &self.skill_path,
            format!("---\nname: demo\ndescription: {description}\n---\nInstructions.\n"),
        )
        .unwrap();
    }

    async fn snapshot(&self) -> HostSkillsSnapshot {
        self.service
            .snapshot_for_config(&self.input, Some(self.file_system.clone()))
            .await
    }
}

fn description(snapshot: &HostSkillsSnapshot) -> &str {
    assert!(snapshot.outcome().errors.is_empty());
    assert_eq!(snapshot.outcome().skills.len(), 1);
    &snapshot.outcome().skills[0].description
}

#[tokio::test]
async fn reproduce_forced_refresh_isolation() {
    let mut stale = 0;
    for _ in 0..CACHE_ATTEMPTS {
        let fixture = ServiceFixture::new();
        assert_eq!(description(&fixture.snapshot().await), "before");
        fixture.write("after");
        let refreshed = fixture
            .service
            .snapshot_for_cwd(&fixture.input, true, Some(fixture.file_system.clone()))
            .await;
        assert_eq!(description(&refreshed), "after");
        let model_snapshot = fixture.snapshot().await;
        stale += usize::from(description(&model_snapshot) == "before");
        fixture.service.clear_cache();
        assert_eq!(
            description(&fixture.snapshot().await),
            "after",
            "control must read the real updated file"
        );
    }
    println!(
        "SKILLS_PROBE {{\"case\":\"forced_refresh\",\"attempts\":{CACHE_ATTEMPTS},\"stale_model_snapshots\":{stale},\"fresh_list_controls\":{CACHE_ATTEMPTS},\"fresh_clear_controls\":{CACHE_ATTEMPTS}}}"
    );
    assert_eq!(
        stale, 0,
        "forced refresh must refresh subsequent model-facing snapshots"
    );
}

#[tokio::test]
async fn reproduce_invalidation_race() {
    let mut stale = 0;
    for _ in 0..CACHE_ATTEMPTS {
        let fixture = ServiceFixture::new();
        let gate = Arc::new(ReadGate::default());
        *fixture.file_system.read_gate.lock().unwrap() = Some(Arc::clone(&gate));
        let (older, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(fixture.snapshot(), async {
                gate.started.notified().await;
                fixture.write("after");
                fixture.service.clear_cache();
                assert_eq!(
                    description(&fixture.snapshot().await),
                    "after",
                    "new generation must publish first"
                );
                gate.release.notify_one();
            })
        })
        .await
        .expect("barrier-controlled scan must complete");
        assert_eq!(description(&older), "before");
        stale += usize::from(description(&fixture.snapshot().await) == "before");
        fixture.service.clear_cache();
        assert_eq!(description(&fixture.snapshot().await), "after");
    }
    println!(
        "SKILLS_PROBE {{\"case\":\"invalidation_race\",\"attempts\":{CACHE_ATTEMPTS},\"stale_cache_overwrites\":{stale},\"fresh_newer_load_controls\":{CACHE_ATTEMPTS},\"fresh_clear_controls\":{CACHE_ATTEMPTS}}}"
    );
    assert_eq!(
        stale, 0,
        "an older in-flight scan must not replace the newer cached snapshot"
    );
}
