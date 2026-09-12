use std::io;
use std::sync::LazyLock;

use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::key_hint::KeyBindingListExt;
use codex_model_provider_info::DEFAULT_LMSTUDIO_PORT;
use codex_model_provider_info::DEFAULT_OLLAMA_PORT;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::{self};
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::time::Duration;

#[derive(Clone)]
struct ProviderOption {
    name: String,
    status: ProviderStatus,
}

#[derive(Clone)]
enum ProviderStatus {
    Running,
    NotRunning,
    Unknown,
}

/// Options displayed in the *select* mode.
///
/// The `key` is matched case-insensitively.
struct SelectOption {
    label: Line<'static>,
    description: &'static str,
    key: KeyCode,
    provider_id: &'static str,
}

static OSS_SELECT_OPTIONS: LazyLock<Vec<SelectOption>> = LazyLock::new(|| {
    vec![
        SelectOption {
            label: Line::from(vec!["L".underlined(), "M Studio".into()]),
            description: "Local LM Studio server (default port 1234)",
            key: KeyCode::Char('l'),
            provider_id: LMSTUDIO_OSS_PROVIDER_ID,
        },
        SelectOption {
            label: Line::from(vec!["O".underlined(), "llama".into()]),
            description: "Local Ollama server (Responses API, default port 11434)",
            key: KeyCode::Char('o'),
            provider_id: OLLAMA_OSS_PROVIDER_ID,
        },
    ]
});

// This startup wizard runs before the main TUI runtime keymap is available, so
// it mirrors the built-in horizontal list defaults instead of reading config.
// The shared matcher still covers raw C0 Ctrl-H/Ctrl-L terminal reports.
const MOVE_LEFT_KEYS: [KeyBinding; 2] = [
    key_hint::plain(KeyCode::Left),
    key_hint::ctrl(KeyCode::Char('h')),
];
const MOVE_RIGHT_KEYS: [KeyBinding; 2] = [
    key_hint::plain(KeyCode::Right),
    key_hint::ctrl(KeyCode::Char('l')),
];

pub struct OssSelectionWidget<'a> {
    select_options: &'a Vec<SelectOption>,
    confirmation_prompt: Paragraph<'a>,

    /// Currently selected index in *select* mode.
    selected_option: usize,

    /// Set to `true` once a decision has been sent – the parent view can then
    /// remove this widget from its queue.
    done: bool,

    selection: Option<String>,
}

impl OssSelectionWidget<'_> {
    fn new(lmstudio_status: ProviderStatus, ollama_status: ProviderStatus) -> io::Result<Self> {
        let providers = vec![
            ProviderOption {
                name: "LM Studio".to_string(),
                status: lmstudio_status,
            },
            ProviderOption {
                name: "Ollama (Responses)".to_string(),
                status: ollama_status.clone(),
            },
            ProviderOption {
                name: "Ollama (Chat)".to_string(),
                status: ollama_status,
            },
        ];

        let mut contents: Vec<Line> = vec![
            Line::from(vec![
                "? ".fg(Color::Blue),
                "Select an open-source provider".bold(),
            ]),
            Line::from(""),
            Line::from("  Choose which local AI server to use for your session."),
            Line::from(""),
        ];

        // Add status indicators for each provider
        for provider in &providers {
            let (status_symbol, status_color) = get_status_symbol_and_color(&provider.status);
            contents.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(status_symbol, Style::default().fg(status_color)),
                Span::raw(format!(" {} ", provider.name)),
            ]));
        }
        contents.push(Line::from(""));
        contents.push(Line::from("  ● Running  ○ Not Running").add_modifier(Modifier::DIM));

        contents.push(Line::from(""));
        contents.push(
            Line::from("  Press Enter to select • Ctrl+C to exit").add_modifier(Modifier::DIM),
        );

        let confirmation_prompt = Paragraph::new(contents).wrap(Wrap { trim: false });

        Ok(Self {
            select_options: &OSS_SELECT_OPTIONS,
            confirmation_prompt,
            selected_option: 0,
            done: false,
            selection: None,
        })
    }

    fn get_confirmation_prompt_height(&self, width: u16) -> u16 {
        // Should cache this for last value of width.
        self.confirmation_prompt.line_count(width) as u16
    }

    /// Process a `KeyEvent` coming from crossterm. Always consumes the event
    /// while the modal is visible.
    /// Process a key event originating from crossterm. As the modal fully
    /// captures input while visible, we don't need to report whether the event
    /// was consumed—callers can assume it always is.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Option<String> {
        if key.kind == KeyEventKind::Press {
            self.handle_select_key(key);
        }
        if self.done {
            self.selection.clone()
        } else {
            None
        }
    }

    /// Normalize a key for comparison.
    /// - For `KeyCode::Char`, converts to lowercase for case-insensitive matching.
    /// - Other key codes are returned unchanged.
    fn normalize_keycode(code: KeyCode) -> KeyCode {
        match code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        }
    }

    fn handle_select_key(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                self.send_decision("__CANCELLED__".to_string());
            }
            _ if MOVE_LEFT_KEYS.is_pressed(key_event) => {
                self.selected_option = (self.selected_option + self.select_options.len() - 1)
                    % self.select_options.len();
            }
            _ if MOVE_RIGHT_KEYS.is_pressed(key_event) => {
                self.selected_option = (self.selected_option + 1) % self.select_options.len();
            }
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => {
                let opt = &self.select_options[self.selected_option];
                self.send_decision(opt.provider_id.to_string());
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.send_decision(LMSTUDIO_OSS_PROVIDER_ID.to_string());
            }
            KeyEvent { code, .. } => {
                let other = code;
                let normalized = Self::normalize_keycode(other);
                if let Some(opt) = self
                    .select_options
                    .iter()
                    .find(|opt| Self::normalize_keycode(opt.key) == normalized)
                {
                    self.send_decision(opt.provider_id.to_string());
                }
            }
        }
    }

    fn send_decision(&mut self, selection: String) {
        self.selection = Some(selection);
        self.done = true;
    }

    /// Returns `true` once the user has made a decision and the widget no
    /// longer needs to be displayed.
    pub fn is_complete(&self) -> bool {
        self.done
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.get_confirmation_prompt_height(width) + self.select_options.len() as u16
    }
}

impl WidgetRef for &OssSelectionWidget<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let prompt_height = self.get_confirmation_prompt_height(area.width);
        let [prompt_chunk, response_chunk] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(prompt_height), Constraint::Min(0)])
            .areas(area);

        let lines: Vec<Line> = self
            .select_options
            .iter()
            .enumerate()
            .map(|(idx, opt)| {
                let style = if idx == self.selected_option {
                    Style::new().bg(Color::Cyan).fg(Color::Black)
                } else {
                    Style::new().bg(Color::DarkGray)
                };
                opt.label.clone().alignment(Alignment::Center).style(style)
            })
            .collect();

        let [title_area, button_area, description_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(response_chunk.inner(Margin::new(1, 0)));

        Line::from("Select provider?").render(title_area, buf);

        self.confirmation_prompt.clone().render(prompt_chunk, buf);
        let areas = Layout::horizontal(
            lines
                .iter()
                .map(|l| Constraint::Length(l.width() as u16 + 2)),
        )
        .spacing(1)
        .split(button_area);
        for (idx, area) in areas.iter().enumerate() {
            let line = &lines[idx];
            line.render(*area, buf);
        }

        Line::from(self.select_options[self.selected_option].description)
            .style(Style::new().italic().fg(Color::DarkGray))
            .render(description_area.inner(Margin::new(1, 0)), buf);
    }
}

fn get_status_symbol_and_color(status: &ProviderStatus) -> (&'static str, Color) {
    match status {
        ProviderStatus::Running => ("●", Color::Green),
        ProviderStatus::NotRunning => ("○", Color::Red),
        ProviderStatus::Unknown => ("?", Color::Yellow),
    }
}

pub(crate) struct OssProviderSelection {
    pub(crate) provider: String,
    pub(crate) manually_selected: bool,
}

pub async fn select_oss_provider() -> io::Result<OssProviderSelection> {
    // Check provider statuses first
    let lmstudio_status = check_lmstudio_status().await;
    let ollama_status = check_ollama_status().await;

    // Autoselect if only one is running
    match (&lmstudio_status, &ollama_status) {
        (ProviderStatus::Running, ProviderStatus::NotRunning) => {
            let provider = LMSTUDIO_OSS_PROVIDER_ID.to_string();
            return Ok(OssProviderSelection {
                provider,
                manually_selected: false,
            });
        }
        (ProviderStatus::NotRunning, ProviderStatus::Running) => {
            let provider = OLLAMA_OSS_PROVIDER_ID.to_string();
            return Ok(OssProviderSelection {
                provider,
                manually_selected: false,
            });
        }
        _ => {
            // Both running or both not running - show UI
        }
    }

    let mut widget = OssSelectionWidget::new(lmstudio_status, ollama_status)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = loop {
        terminal.draw(|f| {
            (&widget).render_ref(f.area(), f.buffer_mut());
        })?;

        if let Event::Key(key_event) = event::read()?
            && let Some(selection) = widget.handle_key_event(key_event)
        {
            break Ok(OssProviderSelection {
                provider: selection,
                manually_selected: true,
            });
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

async fn check_lmstudio_status() -> ProviderStatus {
    match check_port_status(DEFAULT_LMSTUDIO_PORT).await {
        Ok(true) => ProviderStatus::Running,
        Ok(false) => ProviderStatus::NotRunning,
        Err(_) => ProviderStatus::Unknown,
    }
}

async fn check_ollama_status() -> ProviderStatus {
    match check_port_status(DEFAULT_OLLAMA_PORT).await {
        Ok(true) => ProviderStatus::Running,
        Ok(false) => ProviderStatus::NotRunning,
        Err(_) => ProviderStatus::Unknown,
    }
}

async fn check_port_status(port: u16) -> io::Result<bool> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let client = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(|| codex_http_client::HttpClientBuilder::new().build_direct()),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "OSS provider client construction timed out",
        )
    })?
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;

    // Client construction can finish after the deadline before this task is polled again.
    // Do not start a request when construction has already consumed the probe budget.
    if tokio::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "OSS provider client construction timed out",
        ));
    }

    let url = format!("http://localhost:{port}");
    match tokio::time::timeout_at(deadline, client.get(&url).send()).await {
        Ok(Ok(response)) => Ok(response.status().is_success()),
        Ok(Err(_)) | Err(_) => Ok(false), // Connection failed = not running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    #[test]
    fn ctrl_h_l_move_provider_selection() {
        let mut widget = OssSelectionWidget::new(ProviderStatus::Unknown, ProviderStatus::Unknown)
            .expect("widget should initialize");

        assert_eq!(widget.selected_option, 0);
        widget.handle_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(widget.selected_option, 1);
        widget.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(widget.selected_option, 0);
    }

    #[tokio::test]
    async fn check_port_status_uses_shared_http_client_for_loopback_probe() {
        for (status, expected_running) in [(204, true), (500, false)] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;

            assert_eq!(
                check_port_status(server.address().port()).await.unwrap(),
                expected_running
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method.as_str(), "GET");
            assert_eq!(requests[0].url.path(), "/");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn check_port_status_bounds_configured_ca_construction() -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        use tokio::net::windows::named_pipe::ServerOptions;

        const CASE_ENV: &str = "CODEX_TUI_CA_PROBE_TEST_CASE";
        const TEST_NAME: &str =
            "oss_selection::tests::check_port_status_bounds_configured_ca_construction";
        let Ok(case) = std::env::var(CASE_ENV) else {
            let fixtures = tempfile::tempdir()?;
            let invalid_ca = fixtures.path().join("invalid-selected-ca.pem");
            std::fs::write(&invalid_ca, b"not a PEM certificate")?;
            for case in [
                "valid-204",
                "valid-500",
                "invalid",
                "caller-timeout",
                "internal-timeout",
                "shared-deadline",
            ] {
                let selected_ca = if case == "invalid" {
                    invalid_ca.clone()
                } else {
                    std::path::PathBuf::from(format!(
                        r"\\.\pipe\codex-oss-ca-{}",
                        uuid::Uuid::new_v4()
                    ))
                };
                let mut command = tokio::process::Command::new(std::env::current_exe()?);
                command
                    .arg("--exact")
                    .arg(TEST_NAME)
                    .arg("--nocapture")
                    .arg("--test-threads=1")
                    .env_remove("CODEX_CA_CERTIFICATE")
                    .env_remove("SSL_CERT_FILE")
                    .env("CODEX_CA_CERTIFICATE", selected_ca)
                    // The selected Codex CA must win over this invalid fallback.
                    .env("SSL_CERT_FILE", &invalid_ca)
                    .env(CASE_ENV, case)
                    .kill_on_drop(true);
                let output =
                    tokio::time::timeout(Duration::from_secs(20), command.output()).await??;
                assert!(
                    output.status.success(),
                    "{case} failed\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return Ok(());
        };

        let server = MockServer::start().await;
        let status = if case == "valid-500" { 500 } else { 204 };
        let response = if case == "shared-deadline" {
            ResponseTemplate::new(status).set_delay(Duration::from_millis(1_500))
        } else {
            ResponseTemplate::new(status)
        };
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(&server)
            .await;
        let port = server.address().port();
        if case == "invalid" {
            let error = check_port_status(port)
                .await
                .expect_err("the selected invalid CA must reject construction before any GET");
            assert!(error.to_string().contains("invalid-selected-ca.pem"));
            assert!(server.received_requests().await.unwrap().is_empty());
            return Ok(());
        }

        let ca_path = std::env::var("CODEX_CA_CERTIFICATE")?;
        let mut ca_pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .create(ca_path)?;
        let (read_started_tx, read_started_rx) = tokio::sync::oneshot::channel();
        let (release_ca_tx, release_ca_rx) = tokio::sync::oneshot::channel();
        let pipe_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), ca_pipe.connect()).await??;
            let _ = read_started_tx.send(());
            // This watchdog runs outside the probe's current-thread runtime.
            // Even an unfixed blocking CA read is released so the test can fail.
            let released = tokio::time::timeout(Duration::from_secs(5), release_ca_rx).await;
            ca_pipe
                .write_all(include_bytes!(
                    "../../http-client/tests/fixtures/test-ca.pem"
                ))
                .await?;
            tokio::task::spawn_blocking(move || {
                use std::os::windows::io::AsRawHandle;
                // The owned pipe stays alive until its bytes have been read;
                // closing an unread Windows pipe can discard its buffered data.
                // SAFETY: ca_pipe owns this live handle for the entire call.
                let flushed = unsafe {
                    windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(
                        ca_pipe.as_raw_handle(),
                    )
                };
                if flushed == 0 {
                    return Err(io::Error::last_os_error());
                }
                drop(ca_pipe);
                Ok(())
            })
            .await??;
            released??;
            Ok::<(), anyhow::Error>(())
        });
        let probe_case = case.clone();
        let probe_thread = std::thread::spawn(move || -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(async move {
                let mut probe = Box::pin(check_port_status(port));
                tokio::select! {
                    biased;
                    result = &mut probe => {
                        anyhow::bail!("probe finished before selected CA bytes were released: {result:?}");
                    }
                    result = read_started_rx => result?,
                }
                // The pipe connection proves the real process CA file was opened.
                // This timer must progress while fs::read is still waiting for bytes.
                tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::time::sleep(Duration::from_millis(20)),
                )
                .await?;
                match probe_case.as_str() {
                    "valid-204" | "valid-500" => {
                        let _ = release_ca_tx.send(());
                        assert_eq!(probe.await?, probe_case == "valid-204");
                    }
                    "shared-deadline" => {
                        // A successful but slow CA read consumes half the total
                        // budget; the HTTP response cannot receive a new two seconds.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let _ = release_ca_tx.send(());
                        assert!(!probe.await?, "construction and GET must share one deadline");
                    }
                    "caller-timeout" => {
                        tokio::time::timeout(Duration::from_millis(100), &mut probe)
                            .await
                            .expect_err("the caller deadline must include blocked construction");
                        drop(probe);
                        let _ = release_ca_tx.send(());
                    }
                    "internal-timeout" => {
                        let error = tokio::time::timeout(Duration::from_secs(3), &mut probe)
                            .await?
                            .expect_err("the internal probe deadline must include CA construction");
                        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                        drop(probe);
                        let _ = release_ca_tx.send(());
                    }
                    other => anyhow::bail!("unexpected isolated CA case: {other}"),
                }
                Ok(())
            });
            // Wait for any abandoned client-construction worker to finish before
            // the outer runtime checks whether it sent a late HTTP request.
            drop(runtime);
            result
        });
        let probe_result = tokio::task::spawn_blocking(move || probe_thread.join())
            .await?
            .expect("probe thread panicked");
        let pipe_result = pipe_task.await?;
        probe_result?;
        pipe_result?;
        let requests = server.received_requests().await.unwrap();
        if case.starts_with("valid-") || case == "shared-deadline" {
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method.as_str(), "GET");
            assert_eq!(requests[0].url.path(), "/");
        } else {
            assert!(
                requests.is_empty(),
                "a timed-out construction must never send a late GET"
            );
        }
        Ok(())
    }
}
