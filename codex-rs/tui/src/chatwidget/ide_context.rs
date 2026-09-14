//! Chat-widget wiring for the `/ide` command and IDE context prompt injection.

use codex_app_server_protocol::UserInput;

use super::ChatWidget;

#[derive(Default)]
pub(super) struct IdeContextState {
    enabled: bool,
    prompt_fetch_warned: bool,
}

impl IdeContextState {
    pub(super) fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn enable(&mut self) {
        self.enabled = true;
        self.prompt_fetch_warned = false;
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.prompt_fetch_warned = false;
    }

    fn mark_available(&mut self) {
        self.prompt_fetch_warned = false;
    }
}

impl ChatWidget {
    pub(super) fn handle_ide_command(&mut self) {
        self.handle_ide_command_args("");
    }

    pub(super) fn handle_ide_command_args(&mut self, args: &str) {
        self.handle_ide_command_args_with_fetch(args, |cwd| {
            crate::ide_context::fetch_ide_context(cwd).map_err(|err| err.user_facing_hint())
        });
    }

    pub(super) fn handle_ide_command_args_with_fetch(
        &mut self,
        args: &str,
        fetch: impl FnOnce(&std::path::Path) -> Result<crate::ide_context::IdeContext, String>,
    ) {
        let args = args.to_ascii_lowercase();
        let args = if args.is_empty() {
            if self.ide_context.is_enabled() {
                "off"
            } else {
                "on"
            }
        } else {
            args.as_str()
        };
        match args {
            "on" => {
                self.ide_context.enable();
                self.add_ide_context_status_message(true, fetch);
            }
            "off" => {
                self.ide_context.disable();
                self.sync_ide_context_status_indicator();
                self.add_info_message("IDE context is off.".to_string(), None);
            }
            "status" => self.add_ide_context_status_message(false, fetch),
            _ => self.add_error_message("Usage: /ide [on|off|status]".to_string()),
        }
    }

    /// Fetches fresh IDE context for the outgoing user turn and folds it into the prompt.
    pub(super) fn maybe_apply_ide_context(&mut self, items: &mut Vec<UserInput>) {
        if !self.ide_context.is_enabled() {
            return;
        }

        match crate::ide_context::fetch_ide_context(&self.config.cwd) {
            Ok(context) => {
                self.ide_context.mark_available();
                self.sync_ide_context_status_indicator();
                crate::ide_context::apply_ide_context_to_user_input(&context, items);
            }
            Err(err) => {
                self.sync_ide_context_status_indicator();
                if !self.ide_context.prompt_fetch_warned {
                    self.ide_context.prompt_fetch_warned = true;
                    self.add_info_message(
                        "IDE context was skipped for this message.".to_string(),
                        Some(err.prompt_skip_hint()),
                    );
                }
            }
        }
    }

    fn add_ide_context_status_message(
        &mut self,
        initial_enablement: bool,
        fetch: impl FnOnce(&std::path::Path) -> Result<crate::ide_context::IdeContext, String>,
    ) {
        if !self.ide_context.is_enabled() {
            self.sync_ide_context_status_indicator();
            self.add_info_message("IDE context is off.".to_string(), /*hint*/ None);
            return;
        }

        match fetch(self.config.cwd.as_path()) {
            Ok(context) => {
                self.ide_context.mark_available();
                self.sync_ide_context_status_indicator();
                if crate::ide_context::has_prompt_context(&context) {
                    self.add_info_message(
                        "IDE context is on.".to_string(),
                        Some(
                            "Future messages will include your current IDE selection and open tabs."
                                .to_string(),
                        ),
                    );
                } else {
                    self.add_info_message(
                        "IDE context is on.".to_string(),
                        Some("Connected to your IDE.".to_string()),
                    );
                }
            }
            Err(err) => {
                if initial_enablement {
                    self.ide_context.disable();
                }
                self.sync_ide_context_status_indicator();
                self.add_info_message(
                    if initial_enablement {
                        "IDE context could not be enabled."
                    } else {
                        "IDE context is on, but currently unavailable."
                    }
                    .to_string(),
                    Some(err),
                );
            }
        }
    }

    pub(super) fn sync_ide_context_status_indicator(&mut self) {
        self.bottom_pane
            .set_ide_context_active(self.ide_context.is_enabled());
    }
}
