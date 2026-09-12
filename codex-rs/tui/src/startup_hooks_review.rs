use color_eyre::eyre::Result;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use tokio::sync::mpsc::unbounded_channel;
use tokio_stream::StreamExt;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::app_server_session::AppServerSession;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::ListSelectionView;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line_for_keymap;
use crate::config_update::format_config_error;
use crate::hooks_rpc::HookTrustUpdate;
use crate::hooks_rpc::fetch_hooks_list;
use crate::hooks_rpc::hook_needs_review;
use crate::hooks_rpc::hooks_list_entry_for_cwd;
use crate::hooks_rpc::write_hook_trusts;
use crate::keymap::RuntimeKeymap;
use crate::legacy_core::config::Config;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::HooksListEntry;
use std::path::PathBuf;

pub(crate) enum StartupHooksReviewOutcome {
    Continue,
    OpenHooksBrowser(HooksListEntry),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupHooksReviewSelection {
    ReviewHooks,
    TrustAllAndContinue,
    ContinueWithoutTrusting,
}

pub(crate) async fn load_startup_hooks_review_entry(
    request_handle: AppServerRequestHandle,
    cwd: PathBuf,
) -> HooksListEntry {
    let response = match fetch_hooks_list(request_handle, cwd.clone()).await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!("failed to load startup hook review state: {err:#}");
            return HooksListEntry {
                cwd,
                hooks: Vec::new(),
                warnings: Vec::new(),
                errors: Vec::new(),
            };
        }
    };
    hooks_list_entry_for_cwd(response, &cwd)
}

pub(crate) async fn maybe_run_startup_hooks_review(
    app_server: &mut AppServerSession,
    tui: &mut Tui,
    config: &Config,
    bypass_hook_trust: bool,
    entry: HooksListEntry,
) -> Result<StartupHooksReviewOutcome> {
    if !review_is_needed(bypass_hook_trust, &entry) {
        return Ok(StartupHooksReviewOutcome::Continue);
    }

    run_startup_hooks_review_app(app_server, tui, config, entry).await
}

async fn run_startup_hooks_review_app(
    app_server: &mut AppServerSession,
    tui: &mut Tui,
    config: &Config,
    entry: HooksListEntry,
) -> Result<StartupHooksReviewOutcome> {
    let keymap = RuntimeKeymap::from_config(&config.tui_keymap)
        .map_err(|err| color_eyre::eyre::eyre!(err))?;
    run_startup_hooks_review_loop(
        app_server.request_handle(),
        &keymap,
        entry,
        tui.event_stream(),
        |view| draw_view(tui, view),
    )
    .await
}

async fn run_startup_hooks_review_loop(
    request_handle: AppServerRequestHandle,
    keymap: &RuntimeKeymap,
    entry: HooksListEntry,
    mut tui_events: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = TuiEvent> + Send>>,
    mut draw: impl FnMut(&ListSelectionView) -> Result<()>,
) -> Result<StartupHooksReviewOutcome> {
    let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(tx_raw);
    let mut trust_all_error = None;
    let mut view = selection_view(
        &entry,
        trust_all_error.as_deref(),
        /*trusting_all*/ false,
        app_event_tx.clone(),
        keymap,
    );
    draw(&view)?;

    loop {
        let Some(event) = tui_events.next().await else {
            return Ok(StartupHooksReviewOutcome::Continue);
        };
        match event {
            TuiEvent::Key(key_event) => {
                if matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    view.handle_key_event(key_event);
                }
                let Some(selection) = selected_choice(&mut view) else {
                    draw(&view)?;
                    continue;
                };
                match selection {
                    StartupHooksReviewSelection::ReviewHooks => {
                        return Ok(StartupHooksReviewOutcome::OpenHooksBrowser(entry));
                    }
                    StartupHooksReviewSelection::ContinueWithoutTrusting => {
                        return Ok(StartupHooksReviewOutcome::Continue);
                    }
                    StartupHooksReviewSelection::TrustAllAndContinue => {
                        view = selection_view(
                            &entry,
                            trust_all_error.as_deref(),
                            /*trusting_all*/ true,
                            app_event_tx.clone(),
                            keymap,
                        );
                        draw(&view)?;
                        let write = write_hook_trusts(
                            request_handle.clone(),
                            entry
                                .hooks
                                .iter()
                                .filter(|hook| hook_needs_review(hook))
                                .map(|hook| HookTrustUpdate {
                                    key: hook.key.clone(),
                                    current_hash: hook.current_hash.clone(),
                                })
                                .collect(),
                        );
                        tokio::pin!(write);
                        let mut events_open = true;
                        let result = loop {
                            tokio::select! {
                                result = &mut write => break result,
                                event = tui_events.next(), if events_open => match event {
                                    Some(TuiEvent::Draw | TuiEvent::Resize) => draw(&view)?,
                                    // Trusting disables actions until the admitted write settles.
                                    Some(TuiEvent::Key(_) | TuiEvent::Paste(_)) => {},
                                    None => events_open = false,
                                }
                            }
                        }
                        .map(|_| ())
                        .map_err(|err| {
                            format!("Failed to trust hooks: {}", format_config_error(&err))
                        });
                        match result {
                            Ok(()) => return Ok(StartupHooksReviewOutcome::Continue),
                            Err(err) => {
                                trust_all_error = Some(err);
                                view = selection_view(
                                    &entry,
                                    trust_all_error.as_deref(),
                                    /*trusting_all*/ false,
                                    app_event_tx.clone(),
                                    keymap,
                                );
                                draw(&view)?;
                            }
                        }
                    }
                }
            }
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw | TuiEvent::Resize => draw(&view)?,
        }
    }
}

fn selected_choice(view: &mut ListSelectionView) -> Option<StartupHooksReviewSelection> {
    if !view.is_complete() {
        return None;
    }
    match view.take_last_selected_index() {
        Some(0) => Some(StartupHooksReviewSelection::ReviewHooks),
        Some(1) => Some(StartupHooksReviewSelection::TrustAllAndContinue),
        Some(2) | None => Some(StartupHooksReviewSelection::ContinueWithoutTrusting),
        Some(_) => None,
    }
}

fn selection_view(
    entry: &HooksListEntry,
    trust_all_error: Option<&str>,
    trusting_all: bool,
    app_event_tx: AppEventSender,
    keymap: &RuntimeKeymap,
) -> ListSelectionView {
    ListSelectionView::new(
        selection_view_params(entry, trust_all_error, trusting_all, keymap),
        app_event_tx,
        keymap.list.clone(),
    )
}

#[allow(clippy::disallowed_methods)]
fn selection_view_params(
    entry: &HooksListEntry,
    trust_all_error: Option<&str>,
    trusting_all: bool,
    keymap: &RuntimeKeymap,
) -> SelectionViewParams {
    let count = review_needed_count(entry);
    let count_line = match count {
        1 => "1 hook is new or changed.".to_string(),
        count => format!("{count} hooks are new or changed."),
    };
    let mut header = ColumnRenderable::new();
    header.push(Line::from("Hooks need review".bold()));
    header.push(Line::from(count_line).yellow());
    header.push(Line::from(
        "Hooks can run outside the sandbox after you trust them.".dim(),
    ));
    if let Some(error) = trust_all_error {
        header.push(Paragraph::new(Line::from(error.to_string()).red()).wrap(Wrap { trim: false }));
    } else if trusting_all {
        header.push(Line::from("Trusting hooks...".dim()));
    }

    SelectionViewParams {
        footer_hint: Some(standard_popup_hint_line_for_keymap(&keymap.list)),
        items: vec![
            selection_item("Review hooks", trusting_all),
            selection_item("Trust all and continue", trusting_all),
            selection_item("Continue without trusting (hooks won't run)", trusting_all),
        ],
        header: Box::new(header),
        ..Default::default()
    }
}

fn review_needed_count(entry: &HooksListEntry) -> usize {
    entry
        .hooks
        .iter()
        .filter(|hook| hook_needs_review(hook))
        .count()
}

fn review_is_needed(bypass_hook_trust: bool, entry: &HooksListEntry) -> bool {
    !bypass_hook_trust && review_needed_count(entry) > 0
}

fn selection_item(name: &str, is_disabled: bool) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        dismiss_on_select: true,
        is_disabled,
        ..Default::default()
    }
}

fn draw_view(tui: &mut Tui, view: &ListSelectionView) -> Result<()> {
    tui.draw(u16::MAX, |frame| {
        let area = frame.area();
        frame.render_widget_ref(Clear, area);
        let view_area = Rect::new(
            area.x,
            area.y,
            area.width,
            view.desired_height(area.width).min(area.height),
        );
        frame.render_widget_ref(&StandaloneSelectionView { view }, view_area);
    })?;
    Ok(())
}

struct StandaloneSelectionView<'a> {
    view: &'a ListSelectionView,
}

impl WidgetRef for &StandaloneSelectionView<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        self.view.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::review_is_needed;
    use super::selection_view;
    use crate::app_event::AppEvent;
    use crate::app_event_sender::AppEventSender;
    use crate::keymap::RuntimeKeymap;
    use crate::render::renderable::Renderable;
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use codex_app_server_protocol::HookEventName;
    use codex_app_server_protocol::HookHandlerType;
    use codex_app_server_protocol::HookMetadata;
    use codex_app_server_protocol::HookSource;
    use codex_app_server_protocol::HookTrustStatus;
    use codex_app_server_protocol::HooksListEntry;
    use insta::assert_snapshot;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn pending_trust_write_redraws_and_preserves_error_then_retry() {
        use crate::tui::TuiEvent;
        use codex_app_server_client::AppServerRequestHandle;
        use codex_app_server_client::RemoteAppServerClient;
        use codex_app_server_client::RemoteAppServerConnectArgs;
        use codex_app_server_client::RemoteAppServerEndpoint;
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;
        use futures::SinkExt;
        use futures::StreamExt;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU16;
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use tokio::sync::mpsc;
        use tokio_tungstenite::tungstenite::Message;

        tokio::time::timeout(Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("ws://{}", listener.local_addr().unwrap());
            let (arrived_tx, mut arrived_rx) = mpsc::channel(2);
            let (release_tx, mut release_rx) = mpsc::channel(2);
            let written_path = test_path_buf("/tmp/config.toml");
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let init = socket.next().await.unwrap().unwrap();
                let init: serde_json::Value = serde_json::from_str(init.to_text().unwrap()).unwrap();
                assert_eq!(init["method"], "initialize");
                socket.send(Message::Text(serde_json::json!({"id":init["id"],"result":{}}).to_string().into())).await.unwrap();
                let initialized = socket.next().await.unwrap().unwrap();
                let initialized: serde_json::Value = serde_json::from_str(initialized.to_text().unwrap()).unwrap();
                assert_eq!(initialized["method"], "initialized");
                for attempt in 0..2 {
                    let request = socket.next().await.unwrap().unwrap();
                    let request: serde_json::Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                    assert_eq!(request["method"], "config/batchWrite");
                    assert_eq!(request["params"]["reloadUserConfig"], true);
                    assert_eq!(request["params"]["edits"], serde_json::json!([{
                        "keyPath":"hooks.state", "mergeStrategy":"upsert", "value":{
                            "path:new":{"trusted_hash":"sha256:path:new"},
                            "path:changed":{"trusted_hash":"sha256:path:changed"}
                        }
                    }]));
                    arrived_tx.send(()).await.unwrap();
                    release_rx.recv().await.unwrap();
                    let response = if attempt == 0 {
                        serde_json::json!({"id":request["id"],"error":{"code":-32000,"message":"trust rejected"}})
                    } else {
                        serde_json::json!({"id":request["id"],"result":{"status":"ok","version":"1","filePath":written_path,"overriddenMetadata":null}})
                    };
                    socket.send(Message::Text(response.to_string().into())).await.unwrap();
                }
                let _ = release_rx.recv().await;
            });
            let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
                endpoint: RemoteAppServerEndpoint::WebSocket { websocket_url: endpoint, auth_token: None },
                client_name: "codex-tui-test".to_string(), client_version: "0.0.0-test".to_string(),
                experimental_api: true, mcp_server_openai_form_elicitation: false,
                opt_out_notification_methods: Vec::new(), channel_capacity: 8,
            }).await.unwrap();
            let (events_tx, events_rx) = mpsc::channel(16);
            let (paint_tx, mut paint_rx) = mpsc::unbounded_channel();
            let width = Arc::new(AtomicU16::new(80));
            let draw_width = width.clone();
            let keymap = RuntimeKeymap::defaults();
            let review = super::run_startup_hooks_review_loop(
                AppServerRequestHandle::Remote(client.request_handle()), &keymap, entry(),
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(events_rx)),
                move |view| {
                    let width = draw_width.load(Ordering::Relaxed);
                    paint_tx.send((width, render_lines(view, width))).unwrap();
                    Ok(())
                },
            );
            tokio::pin!(review);
            let driver = async {
                assert!(paint_rx.recv().await.unwrap().1.contains("Hooks need review"));
                let key = |code| TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
                events_tx.send(key(KeyCode::Down)).await.unwrap();
                paint_rx.recv().await.unwrap();
                events_tx.send(key(KeyCode::Enter)).await.unwrap();
                assert!(paint_rx.recv().await.unwrap().1.contains("Trusting hooks..."));
                arrived_rx.recv().await.unwrap();

                // Escape/Enter cannot bypass the admitted write. Resize still repaints.
                events_tx.send(key(KeyCode::Esc)).await.unwrap();
                events_tx.send(key(KeyCode::Enter)).await.unwrap();
                width.store(37, Ordering::Relaxed);
                events_tx.send(TuiEvent::Resize).await.unwrap();
                let (actual_width, rendered) = paint_rx.recv().await.unwrap();
                assert_eq!(actual_width, 37);
                assert!(rendered.contains("Trusting hooks..."));
                events_tx.send(TuiEvent::Draw).await.unwrap();
                assert!(paint_rx.recv().await.unwrap().1.contains("Trusting hooks..."));
                release_tx.send(()).await.unwrap();
                let error = paint_rx.recv().await.unwrap().1;
                let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(error.contains("Failed to trust hooks"));
                assert!(error.contains("trust rejected"));

                // Retry through the same selection loop and actual typed RPC.
                events_tx.send(key(KeyCode::Down)).await.unwrap();
                paint_rx.recv().await.unwrap();
                events_tx.send(key(KeyCode::Enter)).await.unwrap();
                paint_rx.recv().await.unwrap();
                arrived_rx.recv().await.unwrap();
                release_tx.send(()).await.unwrap();
                // Keep event and backend lifetimes until the real outcome is observed.
                (events_tx, release_tx)
            };
            let (outcome, (events_tx, release_tx)) = tokio::join!(&mut review, driver);
            assert!(matches!(outcome.unwrap(), super::StartupHooksReviewOutcome::Continue));
            drop(events_tx);
            drop(release_tx);
            peer.await.unwrap();
        }).await.expect("held trust reply must not freeze terminal event processing");
    }

    fn hook(key: &str, trust_status: HookTrustStatus) -> HookMetadata {
        HookMetadata {
            key: key.to_string(),
            event_name: HookEventName::PreToolUse,
            handler_type: HookHandlerType::Command,
            is_managed: false,
            matcher: Some("Bash".to_string()),
            command: Some("/tmp/hook.sh".to_string()),
            timeout_sec: 30,
            status_message: None,
            source_path: test_path_buf("/tmp/hooks.json").abs(),
            source: HookSource::User,
            plugin_id: None,
            display_order: 0,
            enabled: false,
            current_hash: format!("sha256:{key}"),
            trust_status,
        }
    }

    fn entry() -> HooksListEntry {
        HooksListEntry {
            cwd: test_path_buf("/tmp"),
            hooks: vec![
                hook("path:new", HookTrustStatus::Untrusted),
                hook("path:changed", HookTrustStatus::Modified),
            ],
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn render_lines(view: &crate::bottom_pane::ListSelectionView, width: u16) -> String {
        let height = view.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);

        (0..area.height)
            .map(|row| {
                let rendered = (0..area.width)
                    .map(|col| {
                        let symbol = buf[(area.x + col, area.y + row)].symbol();
                        if symbol.is_empty() {
                            " ".to_string()
                        } else {
                            symbol.to_string()
                        }
                    })
                    .collect::<String>();
                rendered.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn bypass_hook_trust_suppresses_startup_review() {
        assert!(!review_is_needed(/*bypass_hook_trust*/ true, &entry()));
    }

    #[test]
    fn untrusted_hooks_need_review_without_bypass() {
        assert!(review_is_needed(/*bypass_hook_trust*/ false, &entry()));
    }

    #[test]
    fn renders_prompt() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let keymap = RuntimeKeymap::defaults();
        let view = selection_view(
            &entry(),
            /*trust_all_error*/ None,
            /*trusting_all*/ false,
            AppEventSender::new(tx_raw),
            &keymap,
        );

        assert_snapshot!(
            "startup_hooks_review_prompt",
            render_lines(&view, /*width*/ 80)
        );
    }

    #[test]
    fn renders_prompt_with_trust_error() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let keymap = RuntimeKeymap::defaults();
        let view = selection_view(
            &entry(),
            Some(
                "Failed to trust hooks: config/batchWrite failed in TUI: Invalid configuration: features.fast_mode=true is not supported; allowed set [fast_mode=false]",
            ),
            /*trusting_all*/ false,
            AppEventSender::new(tx_raw),
            &keymap,
        );

        assert_snapshot!(
            "startup_hooks_review_prompt_with_trust_error",
            render_lines(&view, /*width*/ 62)
        );
    }
}
