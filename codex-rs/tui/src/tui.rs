use std::fmt;
use std::future::Future;
use std::io::IsTerminal;
use std::io::Result;
use std::io::Stdout;
use std::io::Write;
use std::io::stdin;
use std::io::stdout;
use std::panic;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::Command;
use crossterm::SynchronizedUpdate;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;

use crossterm::event::KeyEvent;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;

use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::disable_raw_mode;
use ratatui::crossterm::terminal::enable_raw_mode;
use ratatui::layout::Offset;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::text::Line;
use tokio::sync::broadcast;
use tokio_stream::Stream;

pub use self::frame_requester::FrameRequester;
use crate::custom_terminal;
use crate::custom_terminal::Terminal as CustomTerminal;
use crate::insert_history::HistoryLineWrapPolicy;
use crate::notifications::DesktopNotificationBackend;
use crate::notifications::detect_backend;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::plain_hyperlink_lines;
use crate::tui::event_stream::EventBroker;
use crate::tui::event_stream::TuiEventStream;

use codex_config::types::NotificationCondition;
use codex_config::types::NotificationMethod;

mod event_stream;
mod frame_rate_limiter;
mod frame_requester;

mod keyboard_modes;
#[cfg(test)]
pub(crate) mod test_support;

mod windows_console;

/// Target frame interval for UI redraw scheduling.
pub(crate) const TARGET_FRAME_INTERVAL: Duration = frame_rate_limiter::MIN_FRAME_INTERVAL;

/// A type alias for the terminal type used in this application
pub type Terminal = CustomTerminal<CrosstermBackend<Stdout>>;

pub(crate) struct InitializedTerminal {
    pub(crate) terminal: Terminal,
    pub(crate) enhanced_keys_supported: bool,
}

pub(crate) fn running_in_vscode_terminal() -> bool {
    keyboard_modes::running_in_vscode_terminal()
}

fn should_emit_notification(condition: NotificationCondition, terminal_focused: bool) -> bool {
    match condition {
        NotificationCondition::Unfocused => !terminal_focused,
        NotificationCondition::Always => true,
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if let Err(err) = self.clear_ambient_pet_image() {
            tracing::debug!(error = %err, "failed to clear ambient pet image on TUI drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::clear_for_viewport_change;
    use super::should_emit_notification;
    use crate::custom_terminal::Terminal as CustomTerminal;
    use crate::test_backend::VT100Backend;
    use codex_config::types::NotificationCondition;
    use ratatui::layout::Position;
    use ratatui::layout::Rect;
    use serial_test::serial;

    #[test]
    fn unfocused_notification_condition_is_suppressed_when_focused() {
        assert!(!should_emit_notification(
            NotificationCondition::Unfocused,
            /*terminal_focused*/ true
        ));
    }

    #[test]
    fn always_notification_condition_emits_when_focused() {
        assert!(should_emit_notification(
            NotificationCondition::Always,
            /*terminal_focused*/ true
        ));
    }

    #[test]
    fn unfocused_notification_condition_emits_when_unfocused() {
        assert!(should_emit_notification(
            NotificationCondition::Unfocused,
            /*terminal_focused*/ false
        ));
    }

    #[test]
    #[serial]
    fn with_restored_runs_callback_with_fixed_keep_raw_policy() -> std::io::Result<()> {
        use std::sync::Arc;
        use std::time::Duration;
        use windows_sys::Win32::System::Console::*;

        const CHILD: &str = "CODEX_TEST_NATIVE_TERMINAL_HANDOFF";
        const TEST: &str = "tui::tests::with_restored_runs_callback_with_fixed_keep_raw_policy";
        const VT_INPUT: u32 = 0x0200;
        fn input_mode() -> u32 {
            let mut mode = 0;
            assert_ne!(
                unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) },
                0
            );
            mode
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        if let Some(marker) = std::env::var_os(CHILD) {
            // The parent launches this test inside a real ConPTY, isolated from its console.
            assert_ne!(
                unsafe { SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), input_mode() | VT_INPUT) },
                0
            );
            super::set_modes()?;
            assert_eq!(input_mode() & VT_INPUT, 0);
            runtime.block_on(async {
                let mut tui = super::test_support::make_test_tui().expect("native test terminal");
                let mut events = tui.event_stream();
                // Start the real crossterm reader before relinquishing it.
                assert!(tokio::time::timeout(Duration::from_millis(20), futures::StreamExt::next(&mut events)).await.is_err());
                let broker = Arc::clone(&tui.event_broker);
                let broker_guard = broker.lock_state_for_test();
                let mode_guard = super::windows_console::lock_input_modes_for_test();
                let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let handoff = tui.with_restored(|| async {
                    assert_eq!(input_mode() & VT_INPUT, VT_INPUT, "external program sees restored VT input");
                    assert!(crossterm::terminal::is_raw_mode_enabled().expect("raw mode"));
                    entered_tx.send(()).expect("callback entered");
                    release_rx.await.expect("release external program");
                    "completed"
                });
                tokio::pin!(handoff);
                // These are actual production mutexes; no handoff or mode behavior is replaced.
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut handoff).await.is_err());
                assert!(matches!(entered_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                drop(broker_guard);
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut handoff).await.is_err());
                assert!(matches!(entered_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                drop(mode_guard);
                tokio::select! {
                    result = &mut entered_rx => result.expect("external callback starts after restore"),
                    _ = &mut handoff => panic!("handoff returned before external program finished"),
                    _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("external callback did not start"),
                }
                let mode_guard = super::windows_console::lock_input_modes_for_test();
                release_tx.send(()).expect("finish external program");
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut handoff).await.is_err());
                let broker_guard = broker.lock_state_for_test();
                drop(mode_guard);
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut handoff).await.is_err());
                assert_eq!(input_mode() & VT_INPUT, 0, "TUI input mode restored before events resume");
                drop(broker_guard);
                assert_eq!(tokio::time::timeout(Duration::from_secs(2), &mut handoff).await.expect("handoff resumes"), "completed");
                // The same consumer must receive a real native input record after resume.
                let record = INPUT_RECORD {
                    EventType: KEY_EVENT as u16,
                    Event: INPUT_RECORD_0 { KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: 1, wRepeatCount: 1, wVirtualKeyCode: 0x58, wVirtualScanCode: 0,
                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: b'x' as u16 }, dwControlKeyState: 0,
                    } },
                };
                let mut written = 0;
                assert_ne!(unsafe { WriteConsoleInputW(GetStdHandle(STD_INPUT_HANDLE), &record, 1, &mut written) }, 0);
                assert_eq!(written, 1);
                let event = tokio::time::timeout(Duration::from_secs(2), futures::StreamExt::next(&mut events)).await.expect("native event after handoff").expect("event stream remains open");
                assert!(matches!(event, super::TuiEvent::Key(key) if key.code == crossterm::event::KeyCode::Char('x')));
            });
            super::restore_after_exit()?;
            assert_eq!(
                input_mode() & VT_INPUT,
                VT_INPUT,
                "original mode survives balanced handoff"
            );
            std::fs::write(marker, b"native handoff and runtime progress verified")?;
            return Ok(());
        }
        let fixture = tempfile::tempdir()?;
        let marker = fixture.path().join("native-handoff.txt");
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
                result.expect("native handoff deadline"),
                0,
                "{}",
                String::from_utf8_lossy(&output)
            );
        });
        assert_eq!(
            std::fs::read(marker)?,
            b"native handoff and runtime progress verified"
        );
        Ok(())
    }

    #[test]
    fn pending_history_retry_does_not_duplicate_completed_batches() {
        use super::PendingHistoryLines;
        use super::Tui;
        use crate::insert_history::HistoryLineWrapPolicy;
        use crate::terminal_hyperlinks::HyperlinkLine;
        use ratatui::text::Line;

        for (failed_text, expected_pending, expected_before_retry) in [
            ("FIRST", vec!["FIRST", "SECOND", "THIRD"], vec![]),
            ("SECOND", vec!["SECOND", "THIRD"], vec!["FIRST"]),
        ] {
            let mut terminal =
                CustomTerminal::with_options(VT100Backend::new(12, 8)).expect("terminal");
            let viewport = Rect::new(0, 7, 12, 1);
            terminal.set_viewport_area(viewport);
            terminal
                .backend_mut()
                .fail_next_write_of(failed_text.as_bytes());
            let mut pending = ["FIRST", "SECOND", "THIRD"]
                .into_iter()
                .map(|text| PendingHistoryLines {
                    lines: vec![HyperlinkLine::new(Line::from(text))],
                    wrap_policy: HistoryLineWrapPolicy::PreWrap,
                })
                .collect::<Vec<_>>();

            let error = Tui::flush_pending_history_lines(&mut terminal, &mut pending)
                .expect_err("the actual history write must report its external failure");
            assert_eq!(error.kind(), std::io::ErrorKind::Other);
            assert_eq!(error.to_string(), "injected terminal write failure");
            assert_eq!(
                pending
                    .iter()
                    .map(|batch| batch.lines[0].line.to_string())
                    .collect::<Vec<_>>(),
                expected_pending
            );
            let visible_history = |terminal: &CustomTerminal<VT100Backend>| {
                terminal
                    .backend()
                    .vt100()
                    .screen()
                    .rows(0, 12)
                    .filter(|row| !row.trim().is_empty())
                    .collect::<Vec<_>>()
            };
            assert_eq!(visible_history(&terminal), expected_before_retry);
            assert_eq!(terminal.viewport_area, viewport);

            Tui::flush_pending_history_lines(&mut terminal, &mut pending)
                .expect("retry after the external failure clears");
            assert!(pending.is_empty());
            assert_eq!(visible_history(&terminal), ["FIRST", "SECOND", "THIRD"]);
            assert_eq!(terminal.viewport_area, viewport);
            Tui::flush_pending_history_lines(&mut terminal, &mut pending).expect("empty flush");
            assert_eq!(visible_history(&terminal), ["FIRST", "SECOND", "THIRD"]);
        }
    }

    #[test]
    fn first_viewport_change_clears_from_new_viewport_when_old_viewport_is_empty() {
        let width = 12;
        let height = 4;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            CustomTerminal::with_options_and_cursor_position(backend, Position { x: 0, y: 1 })
                .expect("terminal");
        write!(
            terminal.backend_mut(),
            "shell line\r\nstale cells\r\nmore stale"
        )
        .expect("prefill terminal");

        clear_for_viewport_change(
            &mut terminal,
            Rect::new(
                /*x*/ 0,
                /*y*/ 1,
                /*width*/ width,
                /*height*/ height - 1,
            ),
        )
        .expect("clear transition");

        let rows: Vec<String> = terminal
            .backend()
            .vt100()
            .screen()
            .rows(/*start*/ 0, width)
            .collect();
        assert!(
            rows[0].contains("shell line"),
            "expected content before the viewport to remain visible, rows: {rows:?}"
        );
        assert!(
            !rows.iter().skip(1).any(|row| row.contains("stale")),
            "expected stale cells inside the new viewport to be cleared, rows: {rows:?}"
        );
    }
}

pub fn set_modes() -> Result<()> {
    ensure_virtual_terminal_processing()?;

    execute!(stdout(), EnableBracketedPaste)?;

    enable_raw_mode()?;

    if let Err(err) = windows_console::set_input_record_mode() {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableBracketedPaste);
        return Err(err);
    }
    // Enable keyboard enhancement flags so modifiers for keys like Enter are disambiguated.
    // chat_composer.rs is using a keyboard event listener to enter for any modified keys
    // to create a new line that require this.
    // Some terminals (notably legacy Windows consoles) do not support
    // keyboard enhancement flags. Attempt to enable them, but continue
    // gracefully if unsupported.
    keyboard_modes::enable_keyboard_enhancement();

    let _ = execute!(stdout(), DisableFocusChange);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute EnableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute DisableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawModeRestore {
    Disable,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardRestore {
    PopStack,
    ResetAfterExit,
}

fn restore_common(
    raw_mode_restore: RawModeRestore,
    keyboard_restore: KeyboardRestore,
) -> Result<()> {
    let mut first_error = ensure_virtual_terminal_processing().err();

    match keyboard_restore {
        KeyboardRestore::PopStack => keyboard_modes::restore_keyboard_enhancement_stack(),
        KeyboardRestore::ResetAfterExit => keyboard_modes::reset_keyboard_reporting_after_exit(),
    }

    if let Err(err) = execute!(stdout(), DisableBracketedPaste) {
        first_error.get_or_insert(err);
    }
    let _ = execute!(stdout(), DisableFocusChange);
    if matches!(raw_mode_restore, RawModeRestore::Disable)
        && let Err(err) = disable_raw_mode()
    {
        first_error.get_or_insert(err);
    }

    if let Err(err) = windows_console::restore_input_mode() {
        first_error.get_or_insert(err);
    }
    if let Err(err) = execute!(
        stdout(),
        SetCursorStyle::DefaultUserShape,
        crossterm::cursor::Show
    ) {
        first_error.get_or_insert(err);
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Restore the terminal after Codex is exiting.
///
/// Uses a strong keyboard reset so the parent shell recovers even if a
/// terminal missed the stack pop that normally pairs with [`set_modes`].
pub fn restore_after_exit() -> Result<()> {
    restore_common(RawModeRestore::Disable, KeyboardRestore::ResetAfterExit)
}

/// Restore the terminal to its original state, but keep raw mode enabled.
pub fn restore_keep_raw() -> Result<()> {
    restore_common(RawModeRestore::Keep, KeyboardRestore::PopStack)
}

/// Flush the underlying stdin buffer to clear any input that may be buffered at the terminal level.
/// For example, clears any user input that occurred while the crossterm EventStream was dropped.
fn flush_terminal_input_buffer() {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::FlushConsoleInputBuffer;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;

    // SAFETY: GetStdHandle has no pointer preconditions; invalid results are checked below.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        // SAFETY: GetLastError reads thread-local error state and has no preconditions.
        let err = unsafe { GetLastError() };
        tracing::warn!("failed to get stdin handle for flush: error {err}");
        return;
    }

    // SAFETY: handle is a checked, borrowed standard input handle and is not closed here.
    let result = unsafe { FlushConsoleInputBuffer(handle) };
    if result == 0 {
        // SAFETY: GetLastError reads thread-local error state and has no preconditions.
        let err = unsafe { GetLastError() };
        tracing::warn!("failed to flush stdin buffer: error {err}");
    }
}

/// Initialize the terminal (inline viewport; history stays in normal scrollback)
pub(crate) fn init() -> Result<InitializedTerminal> {
    if !stdin().is_terminal() {
        return Err(std::io::Error::other("stdin is not a terminal"));
    }
    if !stdout().is_terminal() {
        return Err(std::io::Error::other("stdout is not a terminal"));
    }
    set_modes()?;

    flush_terminal_input_buffer();

    set_panic_hook();

    let mut backend = CrosstermBackend::new(stdout());

    let cursor_pos = cursor_position_with_crossterm(&mut backend);

    let enhanced_keys_supported =
        !keyboard_modes::keyboard_enhancement_disabled() && detect_keyboard_enhancement_supported();

    probe_windows_default_colors();

    let tui = CustomTerminal::with_options_and_cursor_position(backend, cursor_pos)?;
    Ok(InitializedTerminal {
        terminal: tui,
        enhanced_keys_supported,
    })
}

fn cursor_position_with_crossterm(backend: &mut CrosstermBackend<Stdout>) -> Position {
    backend.get_cursor_position().unwrap_or_else(|err| {
        tracing::warn!("failed to read initial cursor position; defaulting to origin: {err}");
        Position { x: 0, y: 0 }
    })
}

fn detect_keyboard_enhancement_supported() -> bool {
    supports_keyboard_enhancement().unwrap_or(/*default*/ false)
}

fn probe_windows_default_colors() {
    let started_at = std::time::Instant::now();
    match crate::terminal_probe::default_colors(crate::terminal_probe::DEFAULT_TIMEOUT) {
        Ok(colors) => {
            tracing::info!(
                duration_ms = %started_at.elapsed().as_millis(),
                default_colors = colors.is_some(),
                "terminal default color probe completed"
            );
            crate::terminal_palette::set_default_colors_from_startup_probe(colors);
        }
        Err(err) => {
            tracing::warn!(
                duration_ms = %started_at.elapsed().as_millis(),
                "terminal default color probe failed: {err}"
            );
            crate::terminal_palette::set_default_colors_from_startup_probe(/*colors*/ None);
        }
    }
}

fn set_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_after_exit(); // ignore any errors as we are already failing
        hook(panic_info);
    }));
}

#[derive(Clone, Debug)]
pub enum TuiEvent {
    /// A terminal key event after focus, paste, and protocol bookkeeping has been handled.
    Key(KeyEvent),
    /// A bracketed paste payload normalized by the app layer before it reaches the composer.
    Paste(String),
    /// A terminal size notification that should be handled as resize-sensitive draw work.
    ///
    /// Resize is separate from `Draw` so the app can run feature-gated pre-render logic without
    /// changing the default draw path for scheduled frames.
    Resize,
    /// A scheduled repaint that does not necessarily correspond to a terminal size change.
    Draw,
}

pub struct Tui {
    #[cfg(test)]
    pub(crate) thread_switch_clear_error: Option<std::io::ErrorKind>,
    frame_requester: FrameRequester,
    draw_tx: broadcast::Sender<()>,
    event_broker: Arc<EventBroker>,
    pub(crate) terminal: Terminal,
    pending_history_lines: Vec<PendingHistoryLines>,
    ambient_pet_image_state: crate::pets::PetImageRenderState,
    pet_picker_preview_image_state: crate::pets::PetImageRenderState,
    alt_saved_viewport: Option<ratatui::layout::Rect>,
    // True when overlay alt-screen UI is active
    alt_screen_active: Arc<AtomicBool>,
    // True when terminal/tab is focused; updated internally from crossterm events
    terminal_focused: Arc<AtomicBool>,
    enhanced_keys_supported: bool,
    notification_backend: Option<DesktopNotificationBackend>,
    notification_condition: NotificationCondition,
    // When false, enter_alt_screen() becomes a no-op.
    alt_screen_enabled: bool,
}

struct PendingHistoryLines {
    lines: Vec<HyperlinkLine>,
    wrap_policy: HistoryLineWrapPolicy,
}

fn clear_for_viewport_change<B>(terminal: &mut CustomTerminal<B>, new_area: Rect) -> Result<()>
where
    B: Backend + Write,
{
    let clear_position = if terminal.viewport_area.is_empty() {
        new_area.as_position()
    } else {
        terminal.viewport_area.as_position()
    };
    terminal.clear_after_position(clear_position)
}

impl Tui {
    pub(crate) fn new(terminal: Terminal, enhanced_keys_supported: bool) -> Self {
        let (draw_tx, _) = broadcast::channel(1);
        let frame_requester = FrameRequester::new(draw_tx.clone());

        // Cache this to avoid contention with the event reader.
        supports_color::on_cached(supports_color::Stream::Stdout);
        let _ = crate::terminal_palette::default_colors();
        Self {
            frame_requester,
            draw_tx,
            event_broker: Arc::new(EventBroker::new()),
            terminal,
            #[cfg(test)]
            thread_switch_clear_error: None,
            pending_history_lines: vec![],
            ambient_pet_image_state: crate::pets::PetImageRenderState::default(),
            pet_picker_preview_image_state: crate::pets::PetImageRenderState::default(),
            alt_saved_viewport: None,
            alt_screen_active: Arc::new(AtomicBool::new(false)),
            terminal_focused: Arc::new(AtomicBool::new(true)),
            enhanced_keys_supported,
            notification_backend: Some(detect_backend(NotificationMethod::default())),
            notification_condition: NotificationCondition::default(),
            alt_screen_enabled: true,
        }
    }

    pub(crate) fn clear_for_thread_switch(&mut self) -> Result<()> {
        #[cfg(test)]
        if let Some(kind) = self.thread_switch_clear_error.take() {
            return Err(std::io::Error::new(
                kind,
                "thread-switch terminal clear failed",
            ));
        }
        self.terminal.clear_scrollback_and_visible_screen_ansi()?;
        let mut area = self.terminal.viewport_area;
        if area.y > 0 {
            area.y = 0;
            self.terminal.set_viewport_area(area);
        }
        Ok(())
    }

    /// Set whether alternate screen is enabled. When false, enter_alt_screen() becomes a no-op.
    pub fn set_alt_screen_enabled(&mut self, enabled: bool) {
        self.alt_screen_enabled = enabled;
    }

    pub fn set_notification_settings(
        &mut self,
        method: NotificationMethod,
        condition: NotificationCondition,
    ) {
        self.notification_backend = Some(detect_backend(method));
        self.notification_condition = condition;
    }

    pub fn frame_requester(&self) -> FrameRequester {
        self.frame_requester.clone()
    }

    pub fn enhanced_keys_supported(&self) -> bool {
        self.enhanced_keys_supported
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_screen_active.load(Ordering::Relaxed)
    }

    /// Temporarily restore terminal state to run an external interactive program `f`.
    ///
    /// This pauses crossterm's stdin polling by dropping the underlying event stream, restores
    /// terminal modes while keeping raw mode enabled, then re-applies Codex TUI modes before
    /// resuming events.
    pub async fn with_restored<R, F, Fut>(&mut self, f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = R>,
    {
        // Pause crossterm events to avoid stdin conflicts with external program `f`.
        let broker = Arc::clone(&self.event_broker);
        tokio::task::spawn_blocking(move || broker.pause_events())
            .await
            .expect("terminal input pause worker panicked");

        // Leave alt screen if active to avoid conflicts with external program `f`.
        let was_alt_screen = self.is_alt_screen_active();
        if was_alt_screen {
            if let Err(err) = self.leave_alt_screen() {
                tracing::warn!("failed to leave alternate screen before external program: {err}");
            }
        }

        if let Err(err) = tokio::task::spawn_blocking(restore_keep_raw)
            .await
            .unwrap_or_else(|err| Err(std::io::Error::other(err)))
        {
            tracing::warn!("failed to restore terminal modes before external program: {err}");
        }
        let output = f().await;

        tokio::task::spawn_blocking(|| {
            if let Err(err) = set_modes() {
                tracing::warn!("failed to re-enable terminal modes after external program: {err}");
            }
            // Clear keys buffered while the external program owned the terminal.
            flush_terminal_input_buffer();
        })
        .await
        .expect("terminal mode resume worker panicked");

        if was_alt_screen {
            if let Err(err) = self.enter_alt_screen() {
                tracing::warn!("failed to restore alternate screen after external program: {err}");
            }
        }

        let broker = Arc::clone(&self.event_broker);
        tokio::task::spawn_blocking(move || broker.resume_events())
            .await
            .expect("terminal input resume worker panicked");
        output
    }

    /// Emit a desktop notification now if the terminal is unfocused.
    /// Returns true if a notification was posted.
    pub fn notify(&mut self, message: impl AsRef<str>) -> bool {
        let terminal_focused = self.terminal_focused.load(Ordering::Relaxed);
        if !should_emit_notification(self.notification_condition, terminal_focused) {
            return false;
        }

        let Some(backend) = self.notification_backend.as_mut() else {
            return false;
        };

        let message = message.as_ref().to_string();
        match backend.notify(&message) {
            Ok(()) => true,
            Err(err) => {
                let method = backend.method();
                tracing::warn!(
                    error = %err,
                    method = %method,
                    "Failed to emit terminal notification; disabling future notifications"
                );
                self.notification_backend = None;
                false
            }
        }
    }

    pub fn event_stream(&self) -> Pin<Box<dyn Stream<Item = TuiEvent> + Send + 'static>> {
        let stream = TuiEventStream::new(
            self.event_broker.clone(),
            self.draw_tx.subscribe(),
            self.terminal_focused.clone(),
        );
        Box::pin(stream)
    }

    /// Enter alternate screen and expand the viewport to full terminal size, saving the current
    /// inline viewport for restoration when leaving.
    pub fn enter_alt_screen(&mut self) -> Result<()> {
        if !self.alt_screen_enabled {
            return Ok(());
        }
        let size = self.terminal.size()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen)?;
        self.alt_saved_viewport = Some(self.terminal.viewport_area);
        self.alt_screen_active.store(true, Ordering::Relaxed);
        let setup = (|| -> Result<()> {
            // Enable "alternate scroll" so terminals may translate wheel to arrows.
            execute!(self.terminal.backend_mut(), EnableAlternateScroll)?;
            self.terminal.set_viewport_area(ratatui::layout::Rect::new(
                0,
                0,
                size.width,
                size.height,
            ));
            self.terminal.clear()?;
            Ok(())
        })();
        if setup.is_err() {
            // Entry already succeeded: restore the inline screen before returning the failure.
            if let Err(err) = self.leave_alt_screen() {
                tracing::warn!("failed to restore terminal after alternate-screen setup: {err}");
            }
        }
        setup
    }

    /// Leave alternate screen and restore the previously saved inline viewport, if any.
    pub fn leave_alt_screen(&mut self) -> Result<()> {
        if !self.alt_screen_enabled {
            return Ok(());
        }
        // Disable alternate scroll when leaving alt-screen
        let scroll_result = execute!(self.terminal.backend_mut(), DisableAlternateScroll);
        // Still attempt to leave if disabling scroll failed. Keep the saved state if leaving fails.
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        if let Some(saved) = self.alt_saved_viewport.take() {
            self.terminal.set_viewport_area(saved);
        }
        self.alt_screen_active.store(false, Ordering::Relaxed);
        scroll_result?;
        Ok(())
    }

    pub fn insert_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.insert_history_lines_with_wrap_policy(lines, HistoryLineWrapPolicy::PreWrap);
    }

    pub fn insert_history_lines_with_wrap_policy(
        &mut self,
        lines: Vec<Line<'static>>,
        wrap_policy: HistoryLineWrapPolicy,
    ) {
        self.insert_history_hyperlink_lines_with_wrap_policy(
            plain_hyperlink_lines(lines),
            wrap_policy,
        );
    }

    pub(crate) fn insert_history_hyperlink_lines_with_wrap_policy(
        &mut self,
        lines: Vec<HyperlinkLine>,
        wrap_policy: HistoryLineWrapPolicy,
    ) {
        if lines.is_empty() {
            return;
        }
        if let Some(last) = self.pending_history_lines.last_mut()
            && last.wrap_policy == wrap_policy
        {
            last.lines.extend(lines);
        } else {
            self.pending_history_lines
                .push(PendingHistoryLines { lines, wrap_policy });
        }
        self.frame_requester().schedule_frame();
    }

    pub fn clear_pending_history_lines(&mut self) {
        self.pending_history_lines.clear();
    }

    /// Resize the inline viewport for the resize-reflow path.
    ///
    /// Unlike the legacy draw path, this path does not scroll rows above the viewport when the
    /// terminal shrinks. Resize reflow owns rebuilding those rows from transcript source, so
    /// scrolling here would move the viewport once and then replay history into the wrong row.
    fn update_inline_viewport_for_resize_reflow(
        terminal: &mut Terminal,
        height: u16,
    ) -> Result<bool> {
        let size = terminal.size()?;
        let terminal_height_shrank = size.height < terminal.last_known_screen_size.height;
        let terminal_height_grew = size.height > terminal.last_known_screen_size.height;
        let viewport_was_bottom_aligned =
            terminal.viewport_area.bottom() == terminal.last_known_screen_size.height;
        let previous_area = terminal.viewport_area;

        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;
        let mut needs_full_repaint = false;

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if !terminal_height_shrank {
                terminal
                    .backend_mut()
                    .scroll_region_up(0..area.top(), scroll_by)?;
            }
            area.y = size.height - area.height;
        } else if terminal_height_grew && viewport_was_bottom_aligned {
            area.y = size.height - area.height;
        }

        if area != terminal.viewport_area {
            let clear_position = Position::new(/*x*/ 0, previous_area.y.min(area.y));
            terminal.set_viewport_area(area);
            terminal.clear_after_position(clear_position)?;
            needs_full_repaint = true;
        }

        Ok(needs_full_repaint)
    }

    /// Write any buffered history lines above the viewport and clear the buffer.
    fn flush_pending_history_lines<B>(
        terminal: &mut CustomTerminal<B>,
        pending_history_lines: &mut Vec<PendingHistoryLines>,
    ) -> Result<()>
    where
        B: Backend + Write,
    {
        if pending_history_lines.is_empty() {
            return Ok(());
        }

        for (completed, batch) in pending_history_lines.iter().enumerate() {
            if let Err(error) =
                crate::insert_history::insert_history_hyperlink_lines_with_wrap_policy(
                    terminal,
                    batch.lines.clone(),
                    batch.wrap_policy,
                )
            {
                // Completed batches must not be inserted twice on retry. The failed
                // batch may have partially written, so retain it and every later batch.
                drop(pending_history_lines.drain(..completed));
                return Err(error);
            }
        }
        pending_history_lines.clear();
        Ok(())
    }

    pub fn draw(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        // If we are resuming from ^Z, we need to prepare the resume action now so we can apply it
        // in the synchronized update.

        // Precompute any viewport updates that need a cursor-position query before entering
        // the synchronized update, to avoid racing with the event reader.
        let mut pending_viewport_area = self.pending_viewport_area()?;

        ensure_virtual_terminal_processing()?;

        stdout().sync_update(|_| {
            let terminal = &mut self.terminal;
            if let Some(new_area) = pending_viewport_area.take() {
                terminal.set_viewport_area(new_area);
                terminal.clear()?;
            }

            let size = terminal.size()?;

            let mut area = terminal.viewport_area;
            area.height = height.min(size.height);
            area.width = size.width;
            // If the viewport has expanded, scroll everything else up to make room.
            if area.bottom() > size.height {
                terminal
                    .backend_mut()
                    .scroll_region_up(0..area.top(), area.bottom() - size.height)?;
                area.y = size.height - area.height;
            }
            if area != terminal.viewport_area {
                // On startup, the old viewport can still be empty. Clear from the
                // new viewport top so stale shell cells do not show through spaces.
                clear_for_viewport_change(terminal, area)?;
                terminal.set_viewport_area(area);
            }

            Self::flush_pending_history_lines(terminal, &mut self.pending_history_lines)?;

            // Update the y position for suspending so Ctrl-Z can place the cursor correctly.

            terminal.draw(|frame| {
                draw_fn(frame);
            })
        })?
    }

    pub fn draw_ambient_pet_image(
        &mut self,
        request: Option<crate::pets::AmbientPetDraw>,
    ) -> std::result::Result<(), crate::pets::PetImageRenderError> {
        if let Err(err) = ensure_virtual_terminal_processing() {
            return Err(crate::pets::PetImageRenderError::Terminal(err));
        }

        let terminal = &mut self.terminal;
        let state = &mut self.ambient_pet_image_state;
        stdout().sync_update(|_| {
            match crate::pets::render_ambient_pet_image(terminal.backend_mut(), state, request) {
                Ok(()) => Ok(Ok(())),
                Err(crate::pets::PetImageRenderError::Terminal(err)) => Err(err),
                Err(err @ crate::pets::PetImageRenderError::Asset(_)) => Ok(Err(err)),
            }
        })??
    }

    pub fn draw_pet_picker_preview_image(
        &mut self,
        request: Option<crate::pets::AmbientPetDraw>,
    ) -> std::result::Result<(), crate::pets::PetImageRenderError> {
        if let Err(err) = ensure_virtual_terminal_processing() {
            return Err(crate::pets::PetImageRenderError::Terminal(err));
        }

        let terminal = &mut self.terminal;
        let state = &mut self.pet_picker_preview_image_state;
        stdout().sync_update(|_| {
            match crate::pets::render_pet_picker_preview_image(
                terminal.backend_mut(),
                state,
                request,
            ) {
                Ok(()) => Ok(Ok(())),
                Err(crate::pets::PetImageRenderError::Terminal(err)) => Err(err),
                Err(err @ crate::pets::PetImageRenderError::Asset(_)) => Ok(Err(err)),
            }
        })??
    }

    pub fn clear_ambient_pet_image(
        &mut self,
    ) -> std::result::Result<(), crate::pets::PetImageRenderError> {
        if let Err(err) = ensure_virtual_terminal_processing() {
            return Err(crate::pets::PetImageRenderError::Terminal(err));
        }

        crate::pets::render_ambient_pet_image(
            self.terminal.backend_mut(),
            &mut self.ambient_pet_image_state,
            /*request*/ None,
        )
    }

    /// Draw a frame using the resize-reflow viewport and history insertion rules.
    ///
    /// This is the feature-gated counterpart to `draw`. It intentionally skips
    /// `pending_viewport_area`, whose cursor-position heuristic is part of the legacy path, and
    /// instead lets transcript reflow rebuild scrollback before the frame is rendered.
    pub fn draw_with_resize_reflow(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        // If we are resuming from ^Z, we need to prepare the resume action now so we can apply it
        // in the synchronized update.

        ensure_virtual_terminal_processing()?;

        stdout().sync_update(|_| {
            let terminal = &mut self.terminal;
            let needs_full_repaint =
                Self::update_inline_viewport_for_resize_reflow(terminal, height)?;
            Self::flush_pending_history_lines(terminal, &mut self.pending_history_lines)?;

            if needs_full_repaint {
                terminal.invalidate_viewport();
            }

            // Update the y position for suspending so Ctrl-Z can place the cursor correctly.

            terminal.draw(|frame| {
                draw_fn(frame);
            })
        })?
    }

    fn pending_viewport_area(&mut self) -> Result<Option<Rect>> {
        let terminal = &mut self.terminal;
        let screen_size = terminal.size()?;
        let last_known_screen_size = terminal.last_known_screen_size;
        if screen_size != last_known_screen_size
            && let Ok(cursor_pos) = terminal.get_cursor_position()
        {
            let last_known_cursor_pos = terminal.last_known_cursor_pos;
            // If we resized AND the cursor moved, we adjust the viewport area to keep the
            // cursor in the same position. This heuristic keeps the viewport
            // stable across Windows terminal resize reports.
            if cursor_pos.y != last_known_cursor_pos.y {
                let offset = Offset {
                    x: 0,
                    y: cursor_pos.y as i32 - last_known_cursor_pos.y as i32,
                };
                return Ok(Some(terminal.viewport_area.offset(offset)));
            }
        }
        Ok(None)
    }
}

fn ensure_virtual_terminal_processing() -> Result<()> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::ENABLE_PROCESSED_OUTPUT;
    use windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
    use windows_sys::Win32::System::Console::SetConsoleMode;

    fn enable_for_handle(handle: HANDLE) -> Result<()> {
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Ok(());
        }

        let mut mode = 0;
        // SAFETY: The borrowed handle has been checked, and mode is writable for the call.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Ok(());
        }

        let requested = ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        if mode & requested == requested {
            return Ok(());
        }

        // SAFETY: GetConsoleMode validated this borrowed handle; the new value adds console flags.
        if unsafe { SetConsoleMode(handle, mode | requested) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    // SAFETY: GetStdHandle has no pointer preconditions; enable_for_handle checks invalid results.
    let stdout_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    enable_for_handle(stdout_handle)?;

    // SAFETY: GetStdHandle has no pointer preconditions; enable_for_handle checks invalid results.
    let stderr_handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    enable_for_handle(stderr_handle)?;

    Ok(())
}
