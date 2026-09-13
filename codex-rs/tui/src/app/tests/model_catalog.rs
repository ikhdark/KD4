use super::*;
use assert_matches::assert_matches;
use codex_config::types::ModelAvailabilityNuxConfig;
use codex_protocol::openai_models::ModelAvailabilityNux;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

fn all_model_presets() -> Vec<ModelPreset> {
    crate::test_support::TEST_MODEL_PRESETS.clone()
}

fn model_presets_with_test_upgrades() -> Vec<ModelPreset> {
    let mut presets = all_model_presets();
    for model in ["gpt-5.2", "gpt-5.4"] {
        let preset = presets
            .iter_mut()
            .find(|preset| preset.model == model)
            .unwrap_or_else(|| panic!("{model} preset present"));
        preset.upgrade = Some(ModelUpgrade {
            id: "gpt-5.5".to_string(),
            migration_config_key: "hide_test_migration_prompt".to_string(),
            model_link: None,
            upgrade_copy: None,
            migration_markdown: None,
        });
    }
    presets
}

fn model_availability_nux_config(shown_count: &[(&str, u32)]) -> ModelAvailabilityNuxConfig {
    ModelAvailabilityNuxConfig {
        shown_count: shown_count
            .iter()
            .map(|(model, count)| ((*model).to_string(), *count))
            .collect(),
    }
}

fn model_migration_copy_to_plain_text(copy: &crate::model_migration::ModelMigrationCopy) -> String {
    if let Some(markdown) = copy.markdown.as_ref() {
        return markdown.clone();
    }
    let mut s = String::new();
    for span in &copy.heading {
        s.push_str(&span.content);
    }
    s.push('\n');
    s.push('\n');
    for line in &copy.content {
        for span in &line.spans {
            s.push_str(&span.content);
        }
        s.push('\n');
    }
    s
}

#[tokio::test]
async fn model_migration_prompt_only_shows_for_deprecated_models() {
    let seen = BTreeMap::new();
    let presets = model_presets_with_test_upgrades();
    assert!(should_show_model_migration_prompt(
        "gpt-5.2", "gpt-5.5", &seen, &presets
    ));
    assert!(should_show_model_migration_prompt(
        "gpt-5.4", "gpt-5.5", &seen, &presets
    ));
    assert!(!should_show_model_migration_prompt(
        "gpt-5.4", "gpt-5.4", &seen, &presets
    ));
}

#[test]
fn select_model_availability_nux_picks_only_eligible_model() {
    let mut presets = all_model_presets();
    presets.iter_mut().for_each(|preset| {
        preset.availability_nux = None;
    });
    let target = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("target preset present");
    target.availability_nux = Some(ModelAvailabilityNux {
        message: "gpt-5.4 is available".to_string(),
    });

    let selected = select_model_availability_nux(&presets, &model_availability_nux_config(&[]));

    assert_eq!(
        selected,
        Some(StartupTooltipOverride {
            model_slug: "gpt-5.4".to_string(),
            message: "gpt-5.4 is available".to_string(),
        })
    );
}

#[test]
fn select_model_availability_nux_skips_missing_and_exhausted_models() {
    let mut presets = all_model_presets();
    presets.iter_mut().for_each(|preset| {
        preset.availability_nux = None;
    });
    let gpt_5 = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("gpt-5.4 preset present");
    gpt_5.availability_nux = Some(ModelAvailabilityNux {
        message: "gpt-5.4 is available".to_string(),
    });
    let gpt_5_2 = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4-mini")
        .expect("gpt-5.4-mini preset present");
    gpt_5_2.availability_nux = Some(ModelAvailabilityNux {
        message: "gpt-5.4-mini is available".to_string(),
    });

    let selected = select_model_availability_nux(
        &presets,
        &model_availability_nux_config(&[("gpt-5.4", MODEL_AVAILABILITY_NUX_MAX_SHOW_COUNT)]),
    );

    assert_eq!(
        selected,
        Some(StartupTooltipOverride {
            model_slug: "gpt-5.4-mini".to_string(),
            message: "gpt-5.4-mini is available".to_string(),
        })
    );
}

#[test]
fn select_model_availability_nux_uses_existing_model_order_as_priority() {
    let mut presets = all_model_presets();
    presets.iter_mut().for_each(|preset| {
        preset.availability_nux = None;
    });
    let first = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4-mini")
        .expect("gpt-5.4-mini preset present");
    first.availability_nux = Some(ModelAvailabilityNux {
        message: "first".to_string(),
    });
    let second = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("gpt-5.4 preset present");
    second.availability_nux = Some(ModelAvailabilityNux {
        message: "second".to_string(),
    });

    let selected = select_model_availability_nux(&presets, &model_availability_nux_config(&[]));

    assert_eq!(
        selected,
        Some(StartupTooltipOverride {
            model_slug: "gpt-5.4".to_string(),
            message: "second".to_string(),
        })
    );
}

#[test]
fn select_model_availability_nux_returns_none_when_all_models_are_exhausted() {
    let mut presets = all_model_presets();
    presets.iter_mut().for_each(|preset| {
        preset.availability_nux = None;
    });
    let target = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("target preset present");
    target.availability_nux = Some(ModelAvailabilityNux {
        message: "gpt-5.4 is available".to_string(),
    });

    let selected = select_model_availability_nux(
        &presets,
        &model_availability_nux_config(&[("gpt-5.4", MODEL_AVAILABILITY_NUX_MAX_SHOW_COUNT)]),
    );

    assert_eq!(selected, None);
}

#[tokio::test]
async fn prepare_startup_tooltip_override_persists_model_availability_nux_count() {
    let codex_home = tempdir().expect("temp codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    let mut presets = all_model_presets();
    presets.iter_mut().for_each(|preset| {
        preset.availability_nux = None;
    });
    let target = presets
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("target preset present");
    target.availability_nux = Some(ModelAvailabilityNux {
        message: "gpt-5.4 is available".to_string(),
    });

    let tooltip =
        prepare_startup_tooltip_override(&mut config, &presets, /*is_first_run*/ false).await;

    assert_eq!(tooltip.as_deref(), Some("gpt-5.4 is available"));
    assert_eq!(
        config.model_availability_nux.shown_count,
        HashMap::from([("gpt-5.4".to_string(), 1)])
    );

    let reloaded = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("reloaded config");
    assert_eq!(
        reloaded.model_availability_nux.shown_count,
        HashMap::from([("gpt-5.4".to_string(), 1)])
    );
}

#[tokio::test]
async fn accepted_model_migration_persists_target_default_reasoning_effort() {
    let codex_home = tempdir().expect("temp codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    config.model = Some("gpt-5.2".to_string());
    config.model_reasoning_effort = Some(ReasoningEffortConfig::XHigh);

    let (tx_raw, mut rx) = unbounded_channel();
    let app_event_tx = AppEventSender::new(tx_raw);

    apply_accepted_model_migration(
        &mut config,
        &app_event_tx,
        "gpt-5.2".to_string(),
        "gpt-5.4".to_string(),
        ReasoningEffortConfig::Medium,
    );

    assert_eq!(config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        config.model_reasoning_effort,
        Some(ReasoningEffortConfig::Medium)
    );

    let acknowledged = rx.try_recv().expect("acknowledged event");
    assert_matches!(
        acknowledged,
        AppEvent::PersistModelMigrationPromptAcknowledged { from_model, to_model }
            if from_model == "gpt-5.2" && to_model == "gpt-5.4"
    );

    let update_model = rx.try_recv().expect("update model event");
    assert_matches!(
        update_model,
        AppEvent::UpdateModel(model) if model == "gpt-5.4"
    );

    let update_effort = rx.try_recv().expect("update effort event");
    assert_matches!(
        update_effort,
        AppEvent::UpdateReasoningEffort(Some(ReasoningEffortConfig::Medium))
    );

    let persist_selection = rx.try_recv().expect("persist model selection event");
    assert_matches!(
        persist_selection,
        AppEvent::PersistModelSelection { model, effort }
            if model == "gpt-5.4" && effort == Some(ReasoningEffortConfig::Medium)
    );
}

#[tokio::test]
async fn model_migration_prompt_respects_seen_mapping_and_self_target() {
    let mut seen = BTreeMap::new();
    seen.insert("gpt-5.2".to_string(), "gpt-5.4".to_string());
    assert!(!should_show_model_migration_prompt(
        "gpt-5.2",
        "gpt-5.4",
        &seen,
        &all_model_presets()
    ));
    assert!(!should_show_model_migration_prompt(
        "gpt-5.4",
        "gpt-5.4",
        &seen,
        &all_model_presets()
    ));
}

#[tokio::test]
async fn model_migration_prompt_skips_when_target_missing_or_hidden() {
    let mut available = all_model_presets();
    let mut current = available
        .iter()
        .find(|preset| preset.model == "gpt-5.2")
        .cloned()
        .expect("preset present");
    current.upgrade = Some(ModelUpgrade {
        id: "missing-target".to_string(),
        migration_config_key: "unused-test-key".to_string(),
        model_link: None,
        upgrade_copy: None,
        migration_markdown: None,
    });
    available.retain(|preset| preset.model != "gpt-5.2");
    available.push(current.clone());

    assert!(!should_show_model_migration_prompt(
        &current.model,
        "missing-target",
        &BTreeMap::new(),
        &available,
    ));

    assert!(target_preset_for_upgrade(&available, "missing-target").is_none());

    let mut with_hidden_target = all_model_presets();
    let target = with_hidden_target
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.4")
        .expect("target preset present");
    target.show_in_picker = false;

    assert!(!should_show_model_migration_prompt(
        "gpt-5.2",
        "gpt-5.4",
        &BTreeMap::new(),
        &with_hidden_target,
    ));
    assert!(target_preset_for_upgrade(&with_hidden_target, "gpt-5.4").is_none());
}

#[tokio::test]
async fn model_migration_prompt_shows_for_hidden_model() {
    let codex_home = tempdir().expect("temp codex home");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");

    let mut available_models = model_presets_with_test_upgrades();
    let current = available_models
        .iter_mut()
        .find(|preset| preset.model == "gpt-5.2")
        .expect("gpt-5.2 preset present");
    current.show_in_picker = false;
    let current = current.clone();
    assert!(
        !current.show_in_picker,
        "expected gpt-5.2 to be hidden from picker for this test"
    );

    let upgrade = current.upgrade.as_ref().expect("upgrade configured");
    available_models
        .iter_mut()
        .find(|preset| preset.model == upgrade.id)
        .expect("upgrade target present")
        .show_in_picker = true;
    assert!(
        should_show_model_migration_prompt(
            &current.model,
            &upgrade.id,
            &config.notices.model_migrations,
            &available_models,
        ),
        "expected migration prompt to be eligible for hidden model"
    );

    let target =
        target_preset_for_upgrade(&available_models, &upgrade.id).expect("upgrade target present");
    let target_description = (!target.description.is_empty()).then(|| target.description.clone());
    let can_opt_out = true;
    let copy = migration_copy_for_models(
        &current.model,
        &upgrade.id,
        upgrade.model_link.clone(),
        upgrade.upgrade_copy.clone(),
        upgrade.migration_markdown.clone(),
        target.display_name.clone(),
        target_description,
        can_opt_out,
    );

    assert_snapshot!(
        "model_migration_prompt_shows_for_hidden_model",
        model_migration_copy_to_plain_text(&copy)
    );
}

#[test]
#[serial_test::serial]
fn terminal_output_failure_preserves_screen_state_and_aborts_migration() -> std::io::Result<()> {
    use ratatui::layout::Rect;
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
    use windows_sys::Win32::System::Console::SetStdHandle;

    const CHILD: &str = "CODEX_TEST_NATIVE_TERMINAL_OUTPUT_FAILURE";
    const TEST: &str = "app::tests::model_catalog::terminal_output_failure_preserves_screen_state_and_aborts_migration";
    struct ReadOnlyStdout<'a> {
        original: HANDLE,
        _file: &'a std::fs::File,
    }
    impl<'a> ReadOnlyStdout<'a> {
        fn install(file: &'a std::fs::File) -> std::io::Result<Self> {
            std::io::stdout().flush()?;
            // This isolated ConPTY child owns its process handles; the file outlives the guard.
            let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                original,
                _file: file,
            })
        }
    }
    impl Drop for ReadOnlyStdout<'_> {
        fn drop(&mut self) {
            // Restore the still-live console handle before assertions or test harness output.
            unsafe { SetStdHandle(STD_OUTPUT_HANDLE, self.original) };
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if let Some(marker) = std::env::var_os(CHILD) {
        crate::tui::set_modes()?;
        let fixture = tempfile::tempdir()?;
        let path = fixture.path().join("read-only-output");
        std::fs::write(&path, b"")?;
        let file = std::fs::File::open(path)?;
        runtime.block_on(async {
            let mut tui = crate::tui::test_support::make_test_tui().expect("native test terminal");
            let inline = Rect::new(0, 20, 80, 4);
            tui.terminal.set_viewport_area(inline);
            let failed_entry = {
                let _output = ReadOnlyStdout::install(&file).expect("read-only stdout");
                tui.enter_alt_screen()
            };
            assert!(
                failed_entry.is_err(),
                "real output failure reaches the caller"
            );
            assert!(!tui.is_alt_screen_active());
            assert_eq!(tui.terminal.viewport_area, inline);

            tui.enter_alt_screen()
                .expect("working console enters alternate screen");
            assert!(tui.is_alt_screen_active());
            let fullscreen = tui.terminal.viewport_area;
            assert_eq!(fullscreen.y, 0);
            let failed_exit = {
                let _output = ReadOnlyStdout::install(&file).expect("read-only stdout");
                tui.leave_alt_screen()
            };
            assert!(failed_exit.is_err());
            assert!(
                tui.is_alt_screen_active(),
                "failed exit retains state for retry"
            );
            assert_eq!(tui.terminal.viewport_area, fullscreen);
            tui.leave_alt_screen()
                .expect("restored output permits retry");
            assert!(!tui.is_alt_screen_active());
            assert_eq!(tui.terminal.viewport_area, inline);

            let (mut app, mut app_events, mut operations) = Box::pin(make_test_app_with_channels()).await;
            let failed_overlay = {
                let _output = ReadOnlyStdout::install(&file).expect("read-only stdout");
                app.open_transcript_overlay(&mut tui)
            };
            assert!(failed_overlay.is_err());
            assert!(app.overlay.is_none(), "failed entry must not install a transcript overlay");
            assert!(operations.try_recv().is_err(), "failed overlay must not submit an operation");

            app.open_transcript_overlay(&mut tui).expect("working console opens transcript");
            let failed_close = {
                let _output = ReadOnlyStdout::install(&file).expect("read-only stdout");
                app.close_transcript_overlay(&mut tui)
            };
            assert!(failed_close.is_err());
            assert!(app.overlay.is_some(), "failed exit must retain the overlay");
            assert!(operations.try_recv().is_err(), "failed close must not submit an operation");
            app.close_transcript_overlay(&mut tui).expect("working output permits closing retry");
            assert!(app.overlay.is_none());
            assert_eq!(tui.terminal.viewport_area, inline);

            let presets = model_presets_with_test_upgrades();
            for failure in ["entry", "initial draw", "redraw"] {
                // Disabling alt-screen isolates draw failures from entry failure.
                tui.set_alt_screen_enabled(failure == "entry");
                app.config.model = Some("gpt-5.2".to_owned());
                app.config.model_reasoning_effort = Some(ReasoningEffortConfig::XHigh);
                let original_notices = app.config.notices.model_migrations.clone();
                while app_events.try_recv().is_ok() {}
                let requester = tui.frame_requester();
                let result = {
                    let prompt = handle_model_migration_prompt_if_needed(
                        &mut tui, &mut app.config, "gpt-5.2", &app.app_event_tx, &presets,
                    );
                    tokio::pin!(prompt);
                    if failure == "redraw" {
                        assert!(tokio::time::timeout(Duration::from_millis(20), &mut prompt).await.is_err(),
                            "a rendered prompt waits for user input");
                    }
                    let _output = ReadOnlyStdout::install(&file).expect("read-only stdout");
                    requester.schedule_frame();
                    tokio::time::timeout(Duration::from_secs(2), &mut prompt).await
                };
                let exit = result.expect("terminal failure returns without waiting for input")
                    .expect("terminal failure exits startup");
                assert!(matches!(exit.exit_reason, ExitReason::Fatal(message) if message.contains("Failed to display model migration prompt")));
                assert_eq!(app.config.model.as_deref(), Some("gpt-5.2"));
                assert_eq!(app.config.model_reasoning_effort, Some(ReasoningEffortConfig::XHigh));
                assert_eq!(app.config.notices.model_migrations, original_notices);
                assert!(app_events.try_recv().is_err(), "failed migration must not acknowledge, update, or persist model selection");
                assert!(operations.try_recv().is_err(), "failed migration must not submit an operation");
            }

        });
        crate::tui::restore_after_exit()?;
        std::fs::write(marker, b"terminal output failure verified")?;
        return Ok(());
    }
    let fixture = tempfile::tempdir()?;
    let marker = fixture.path().join("output-failure.txt");
    runtime.block_on(async {
        let mut env = std::env::vars().collect::<std::collections::HashMap<_, _>>();
        env.insert(CHILD.to_owned(), marker.to_string_lossy().into_owned());
        let mut child = codex_utils_pty::spawn_pty_process(
            std::env::current_exe()
                .expect("test executable")
                .to_str()
                .expect("UTF-8 executable"),
            &[
                "--exact".to_owned(),
                TEST.to_owned(),
                "--nocapture".to_owned(),
                "--test-threads=1".to_owned(),
            ],
            &std::env::current_dir().expect("working directory"),
            &env,
            &None,
            codex_utils_pty::TerminalSize::default(),
        )
        .await
        .expect("native console child");
        let mut output = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    code = &mut child.exit_rx => break code.expect("native child exit"),
                    Some(bytes) = child.stdout_rx.recv() => output.extend(bytes),
                }
            }
        })
        .await;
        if result.is_err() {
            child
                .session
                .terminate()
                .expect("terminate stuck native child");
        }
        assert_eq!(
            result.expect("native output failure deadline"),
            0,
            "{}",
            String::from_utf8_lossy(&output)
        );
    });
    assert_eq!(std::fs::read(marker)?, b"terminal output failure verified");
    Ok(())
}
