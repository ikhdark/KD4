use super::*;
use codex_app_server_protocol::ExternalAgentConfigImportItemTypeFailure;
use codex_app_server_protocol::ExternalAgentConfigImportItemTypeSuccess;
use codex_app_server_protocol::ExternalAgentConfigImportTypeResult;
use codex_app_server_protocol::ExternalAgentConfigMigrationItemType;
use codex_app_server_protocol::McpServerMigration;
use codex_app_server_protocol::MigrationDetails;
use codex_app_server_protocol::PluginsMigration;
use codex_app_server_protocol::SessionMigration;
use codex_app_server_protocol::SkillMigration;
use pretty_assertions::assert_eq;
use ratatui::text::Line;
use std::path::PathBuf;

fn selected_items() -> Vec<ExternalAgentConfigMigrationItem> {
    vec![
        ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::Config,
            description: "Import settings".to_string(),
            cwd: None,
            details: None,
        },
        ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::Skills,
            description: "Import skills".to_string(),
            cwd: None,
            details: Some(MigrationDetails {
                skills: vec![
                    SkillMigration {
                        name: "triage".to_string(),
                    },
                    SkillMigration {
                        name: "release-notes".to_string(),
                    },
                    SkillMigration {
                        name: "risk-check".to_string(),
                    },
                    SkillMigration {
                        name: "incident-review".to_string(),
                    },
                ],
                ..Default::default()
            }),
        },
        ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::McpServerConfig,
            description: "Import MCP servers".to_string(),
            cwd: None,
            details: Some(MigrationDetails {
                mcp_servers: vec![
                    McpServerMigration {
                        name: "docs".to_string(),
                    },
                    McpServerMigration {
                        name: "issues".to_string(),
                    },
                ],
                ..Default::default()
            }),
        },
        ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::Sessions,
            description: "Import chat sessions".to_string(),
            cwd: None,
            details: Some(MigrationDetails {
                sessions: vec![
                    SessionMigration {
                        path: PathBuf::from("/sessions/alpha.jsonl"),
                        cwd: PathBuf::from("/workspace/project"),
                        title: Some("Alpha rollout".to_string()),
                    },
                    SessionMigration {
                        path: PathBuf::from("/sessions/beta.jsonl"),
                        cwd: PathBuf::from("/workspace/project"),
                        title: Some("Beta review".to_string()),
                    },
                    SessionMigration {
                        path: PathBuf::from("/sessions/gamma.jsonl"),
                        cwd: PathBuf::from("/workspace/project"),
                        title: Some("Gamma notes".to_string()),
                    },
                ],
                ..Default::default()
            }),
        },
        ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::Plugins,
            description: "Import plugins".to_string(),
            cwd: None,
            details: Some(MigrationDetails {
                plugins: vec![PluginsMigration {
                    marketplace_name: "example".to_string(),
                    plugin_names: vec!["formatter".to_string(), "reviewer".to_string()],
                }],
                ..Default::default()
            }),
        },
    ]
}

fn completed_notification() -> ExternalAgentConfigImportCompletedNotification {
    ExternalAgentConfigImportCompletedNotification {
        import_id: "import-1".to_string(),
        item_type_results: vec![
            ExternalAgentConfigImportTypeResult {
                item_type: ExternalAgentConfigMigrationItemType::Config,
                successes: vec![ExternalAgentConfigImportItemTypeSuccess {
                    item_type: ExternalAgentConfigMigrationItemType::Config,
                    cwd: None,
                    source: Some("settings.json".to_string()),
                    target: Some("config.toml".to_string()),
                }],
                failures: Vec::new(),
            },
            ExternalAgentConfigImportTypeResult {
                item_type: ExternalAgentConfigMigrationItemType::Plugins,
                successes: vec![ExternalAgentConfigImportItemTypeSuccess {
                    item_type: ExternalAgentConfigMigrationItemType::Plugins,
                    cwd: None,
                    source: Some("formatter@example".to_string()),
                    target: Some("formatter@example".to_string()),
                }],
                failures: vec![ExternalAgentConfigImportItemTypeFailure {
                    item_type: ExternalAgentConfigMigrationItemType::Plugins,
                    error_type: Some("plugin_install_failed".to_string()),
                    failure_stage: "plugin_import".to_string(),
                    message: "install failed".to_string(),
                    cwd: Some(PathBuf::from("/workspace/project")),
                    source: Some("deployer@example".to_string()),
                }],
            },
        ],
    }
}

#[test]
fn external_agent_config_migration_messages_snapshot() {
    let selected_items = selected_items();
    let completed_notification = completed_notification();
    let messages = [0, 1, 2]
        .into_iter()
        .flat_map(|remaining_item_count| {
            external_agent_config_migration_started_lines(&selected_items, remaining_item_count)
        })
        .chain(external_agent_config_migration_finished_lines(
            &completed_notification,
        ))
        .chain([
            Line::from(EXTERNAL_AGENT_CONFIG_MIGRATION_NO_ITEMS_MESSAGE),
            Line::from(EXTERNAL_AGENT_CONFIG_MIGRATION_REMOTE_UNAVAILABLE_MESSAGE),
            Line::from(EXTERNAL_AGENT_CONFIG_MIGRATION_DAEMON_UNAVAILABLE_MESSAGE),
            Line::from(EXTERNAL_AGENT_CONFIG_IMPORT_IN_PROGRESS_MESSAGE),
        ])
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!("external_agent_config_migration_messages", messages);
}

#[test]
fn external_agent_config_migration_status_lines_use_semantic_colors() {
    assert_eq!(
        external_agent_config_migration_started_lines(
            &selected_items(),
            /*remaining_item_count*/ 0,
        ),
        vec![
            Line::from(vec![
                "• ".dim(),
                "Claude Code import started.".cyan(),
                " You can keep working while it finishes.".into(),
            ]),
            Line::from(vec![
                "  ".into(),
                "Imported setup will apply to new chats.".dim(),
            ]),
            Line::from(vec!["  ".into(), "Importing:".cyan().bold()]),
            Line::from(vec![
                "    ".into(),
                "Settings".cyan(),
                ": ".into(),
                "1".green(),
            ]),
            Line::from(vec![
                "    ".into(),
                "Skills".cyan(),
                ": ".into(),
                "4".green(),
                " — ".dim(),
                "triage, release-notes, risk-check, +1 more".into(),
            ]),
            Line::from(vec![
                "    ".into(),
                "MCP servers".cyan(),
                ": ".into(),
                "2".green(),
                " — ".dim(),
                "docs, issues".into(),
            ]),
            Line::from(vec![
                "    ".into(),
                "Chat sessions".cyan(),
                ": ".into(),
                "3".green(),
                " — ".dim(),
                "Alpha rollout, Beta review, Gamma notes".into(),
            ]),
            Line::from(vec![
                "    ".into(),
                "Plugins".cyan(),
                ": ".into(),
                "2".green(),
                " — ".dim(),
                "formatter, reviewer".into(),
            ]),
        ]
    );

    assert_eq!(
        external_agent_config_migration_finished_lines(&completed_notification()),
        vec![
            Line::from(vec![
                "• ".dim(),
                "Claude Code import finished: ".into(),
                "2 imported".green(),
                ", ".into(),
                "1 failed".red(),
                ".".into(),
            ]),
            Line::from(vec!["  ".into(), "Results by type:".cyan().bold()]),
            Line::from(vec![
                "    ".into(),
                "Settings".cyan(),
                ": ".into(),
                "1 imported".green(),
                ", ".into(),
                "0 failed".green(),
            ]),
            Line::from(vec![
                "    ".into(),
                "Plugins".cyan(),
                ": ".into(),
                "1 imported".green(),
                ", ".into(),
                "1 failed".red(),
            ]),
            Line::from(vec![
                "  ".into(),
                "Run /import again to check for additional items.".dim(),
            ]),
        ]
    );
}

/// Replaces terminal I/O only. The real migration flow, prompt state machine,
/// renderer, configuration detector and embedded app server all remain active.
struct MigrationTestTerminal {
    terminal: crate::custom_terminal::Terminal<crate::test_backend::VT100Backend>,
    events: Vec<crate::tui::TuiEvent>,
    events_polled: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    draw_calls: usize,
    fail_draw: Option<usize>,
}

impl MigrationTestTerminal {
    fn new(events: Vec<crate::tui::TuiEvent>, fail_draw: Option<usize>) -> Self {
        let mut terminal =
            crate::custom_terminal::Terminal::with_screen_size_and_cursor_position_for_test(
                crate::test_backend::VT100Backend::new(80, 24),
                ratatui::layout::Size {
                    width: 80,
                    height: 24,
                },
                ratatui::layout::Position { x: 0, y: 0 },
            );
        terminal.set_viewport_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        Self {
            terminal,
            events,
            events_polled: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            draw_calls: 0,
            fail_draw,
        }
    }
}

impl ExternalAgentConfigMigrationTerminal for MigrationTestTerminal {
    fn frame_requester(&self) -> crate::tui::FrameRequester {
        crate::tui::FrameRequester::test_dummy()
    }

    fn event_stream(
        &mut self,
    ) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = crate::tui::TuiEvent> + Send + 'static>>
    {
        use tokio_stream::StreamExt;
        let events_polled = self.events_polled.clone();
        Box::pin(
            tokio_stream::iter(std::mem::take(&mut self.events)).map(move |event| {
                events_polled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                event
            }),
        )
    }

    fn draw(
        &mut self,
        _height: u16,
        draw_fn: impl FnOnce(&mut crate::custom_terminal::Frame<'_>),
    ) -> std::io::Result<()> {
        self.draw_calls += 1;
        if self.fail_draw == Some(self.draw_calls) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "migration terminal disconnected",
            ));
        }
        self.terminal.draw(draw_fn)
    }
}

#[tokio::test]
async fn migration_flow_draw_errors_stop_before_import() -> color_eyre::Result<()> {
    use crate::legacy_core::config::ConfigBuilder;
    use crate::legacy_core::config::ConfigOverrides;
    use crate::tui::TuiEvent;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use std::sync::atomic::Ordering;

    let temp = tempfile::TempDir::new()?;
    let project = temp.path().join("project");
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(project.join(".git"))?;
    std::fs::create_dir_all(project.join(".claude"))?;
    std::fs::create_dir_all(project.join(".codex"))?;
    std::fs::create_dir_all(&codex_home)?;
    let source_path = project.join(".claude/settings.json");
    let target_path = project.join(".codex/config.toml");
    let source_bytes = br#"{"env":{"MIGRATION_DRAW_TEST":"must-not-be-imported"}}"#;
    let target_bytes = b"# Existing project configuration must be preserved.\n";
    std::fs::write(&source_path, source_bytes)?;
    std::fs::write(&target_path, target_bytes)?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.clone())
        .harness_overrides(ConfigOverrides {
            cwd: Some(project.clone()),
            ..Default::default()
        })
        .build()
        .await?;
    config.sqlite_home = temp.path().join("sqlite");
    let mut app_server = crate::start_embedded_app_server_for_picker(&config).await?;
    let detected = app_server
        .external_agent_config_detect(ExternalAgentConfigDetectParams {
            include_home: false,
            cwds: Some(vec![project.clone()]),
        })
        .await?;
    assert!(
        detected.items.iter().any(|item| item.item_type
            == ExternalAgentConfigMigrationItemType::Config
            && item.cwd.as_deref() == Some(project.as_path())),
        "fixture must expose a real project configuration import"
    );

    for (fail_draw, redraw_event) in [
        (Some(1), None),
        (Some(2), Some(TuiEvent::Draw)),
        (Some(2), Some(TuiEvent::Resize)),
        (None, Some(TuiEvent::Draw)),
    ] {
        let mut events = Vec::new();
        if let Some(event) = redraw_event {
            events.push(event);
        }
        // Keep a regression safe even when home detection finds real items:
        // ignored draw errors consume Escape, never an import confirmation.
        events.push(TuiEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        let mut terminal = MigrationTestTerminal::new(events, fail_draw);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            handle_external_agent_config_migration_prompt(&mut terminal, &mut app_server, &config),
        )
        .await
        .expect("migration flow must terminate");
        if let Some(failed_draw) = fail_draw {
            let message = match outcome {
                Err(message) => message,
                Ok(_) => panic!("failed terminal must return an error, not a migration outcome"),
            };
            assert_eq!(
                message,
                "Could not display the Claude Code import prompt: migration terminal disconnected"
            );
            assert_eq!(terminal.draw_calls, failed_draw);
            assert_eq!(
                terminal.events_polled.load(Ordering::SeqCst),
                failed_draw - 1,
                "draw failure must return before consuming the following Escape"
            );
        } else {
            assert!(matches!(
                outcome,
                Ok(ExternalAgentConfigMigrationFlowOutcome::Cancelled)
            ));
            assert_eq!(terminal.draw_calls, 2);
            assert_eq!(terminal.events_polled.load(Ordering::SeqCst), 2);
        }
        if terminal.draw_calls > 1 {
            assert!(
                terminal
                    .terminal
                    .backend()
                    .to_string()
                    .contains("Bring over your setup"),
                "successful frames must render the actual migration screen"
            );
        }
        assert!(
            !app_server.external_agent_config_import_in_progress(),
            "failure/cancellation must never start an import"
        );
        assert_eq!(std::fs::read(&target_path)?, target_bytes);
        assert_eq!(std::fs::read(&source_path)?, source_bytes);
    }
    app_server.shutdown().await?;
    assert_eq!(
        std::fs::read(&target_path)?,
        target_bytes,
        "no delayed import may change the target during shutdown"
    );
    assert_eq!(std::fs::read(&source_path)?, source_bytes);
    Ok(())
}

#[tokio::test]
async fn migration_prompt_successful_draw_can_confirm_selected_items() {
    use crate::tui::TuiEvent;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let items = selected_items();
    let mut terminal = MigrationTestTerminal::new(
        vec![TuiEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))],
        None,
    );
    let outcome = run_external_agent_config_migration_prompt(&mut terminal, &items, &items, None)
        .await
        .expect("working terminal must permit confirmation");
    assert_eq!(outcome, ExternalAgentConfigMigrationOutcome::Proceed(items));
    assert_eq!(terminal.draw_calls, 1);
    assert!(
        terminal
            .terminal
            .backend()
            .to_string()
            .contains("Bring over your setup")
    );
}
