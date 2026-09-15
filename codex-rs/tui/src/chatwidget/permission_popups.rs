//! Permission and approval popup flows for `ChatWidget`.
//!
//! This module owns the generic permission pickers and confirmation surfaces;
//! Windows-specific sandbox prompting lives beside it in
//! `windows_sandbox_prompts`.

use super::*;

impl ChatWidget {
    /// Open the permissions popup.
    pub(crate) fn open_approvals_popup(&mut self) {
        self.open_permissions_popup();
    }

    /// Open a popup to choose the permissions mode.
    pub(crate) fn open_permissions_popup(&mut self) {
        if self.config.explicit_permission_profile_mode {
            self.open_permission_profiles_popup();
            return;
        }

        let include_read_only = true;
        let current_approval =
            AskForApproval::from(self.config.permissions.approval_policy.value());
        let current_permission_profile = self.config.permissions.permission_profile().clone();
        let mut items: Vec<SelectionItem> = Vec::new();
        let presets: Vec<ApprovalPreset> = builtin_approval_presets();

        let windows_sandbox_level = crate::windows_sandbox::level_from_config(&self.config);

        let windows_degraded_sandbox_enabled =
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);

        let show_elevate_sandbox_hint =
            windows_degraded_sandbox_enabled && presets.iter().any(|preset| preset.id == "auto");

        for preset in presets.into_iter() {
            if !include_read_only && preset.id == "read-only" {
                continue;
            }
            let base_name = if preset.id == "auto" && windows_degraded_sandbox_enabled {
                format!("{ASK_FOR_APPROVAL_LABEL} (non-admin sandbox)")
            } else if preset.id == "auto" {
                ASK_FOR_APPROVAL_LABEL.to_string()
            } else {
                preset.label.to_string()
            };
            let base_description =
                Some(preset.description.replace(" (Identical to Agent mode)", ""));
            let approval_disabled_reason = match self
                .config
                .permissions
                .approval_policy
                .can_set(&preset.approval)
            {
                Ok(()) => None,
                Err(err) => Some(err.to_string()),
            }
            .or_else(|| {
                self.config
                    .permissions
                    .can_set_permission_profile(&preset.permission_profile)
                    .err()
                    .map(|err| err.to_string())
            });
            let default_actions = self.permission_mode_actions(
                &preset,
                base_name.clone(),
                /*profile_selection*/ None,
                /*return_to_permissions*/ !include_read_only,
            );
            items.push(SelectionItem {
                name: base_name,
                description: base_description,
                is_current: Self::preset_matches_current(
                    current_approval,
                    &current_permission_profile,
                    self.config.cwd.as_path(),
                    &preset,
                ),
                actions: default_actions,
                dismiss_on_select: true,
                disabled_reason: approval_disabled_reason,
                ..Default::default()
            });
        }

        let footer_note = show_elevate_sandbox_hint.then(|| {
            vec![
                "The non-admin sandbox protects your files and prevents network access under most circumstances. However, it carries greater risk if prompt injected. To upgrade to the default sandbox, run ".dim(),
                "/setup-default-sandbox".cyan(),
                ".".dim(),
            ]
            .into()
        });

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Update Model Permissions".to_string()),
            footer_note,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(()),
            ..Default::default()
        });
    }

    pub(super) fn approval_preset_actions(
        approval: AskForApproval,
        permission_profile: PermissionProfile,
        active_permission_profile: ActivePermissionProfile,
        label: String,
    ) -> Vec<SelectionAction> {
        vec![Box::new(move |tx| {
            tx.send(AppEvent::CodexOp(AppCommand::override_turn_context(
                /*cwd*/ None,
                Some(approval),
                Some(permission_profile.clone()),
                Some(active_permission_profile.clone()),
                /*windows_sandbox_level*/ None,
                /*model*/ None,
                /*effort*/ None,
                /*summary*/ None,
                /*service_tier*/ None,
                /*collaboration_mode*/ None,
                /*personality*/ None,
            )));
            tx.send(AppEvent::UpdateAskForApprovalPolicy(approval));
            tx.send(AppEvent::UpdateActivePermissionProfile(
                active_permission_profile.clone(),
            ));
            tx.send(AppEvent::InsertHistoryCell(Box::new(
                history_cell::new_info_event(
                    format!("Permissions updated to {label}"),
                    /*hint*/ None,
                ),
            )));
        })]
    }

    pub(super) fn permission_profile_selection_actions(
        selection: PermissionProfileSelection,
    ) -> Vec<SelectionAction> {
        vec![Box::new(move |tx| {
            tx.send(AppEvent::SelectPermissionProfile(selection.clone()));
        })]
    }

    pub(super) fn permission_mode_actions(
        &self,
        preset: &ApprovalPreset,
        label: String,
        profile_selection: Option<PermissionProfileSelection>,
        return_to_permissions: bool,
    ) -> Vec<SelectionAction> {
        let apply_actions = || {
            profile_selection.clone().map_or_else(
                || {
                    Self::approval_preset_actions(
                        AskForApproval::from(preset.approval),
                        preset.permission_profile.clone(),
                        preset.active_permission_profile.clone(),
                        label.clone(),
                    )
                },
                Self::permission_profile_selection_actions,
            )
        };
        let requires_confirmation = preset.id == "full-access";
        if requires_confirmation {
            let preset = preset.clone();
            return vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenFullAccessConfirmation {
                    preset: preset.clone(),
                    return_to_permissions,
                    profile_selection: profile_selection.clone(),
                });
            })];
        }
        if preset.id == "auto" {
            {
                if crate::windows_sandbox::level_from_config(&self.config)
                    == WindowsSandboxLevel::Disabled
                {
                    let preset = preset.clone();
                    return vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenWindowsSandboxEnablePrompt {
                            preset: preset.clone(),
                            profile_selection: profile_selection.clone(),
                        });
                    })];
                }
                if !self
                    .config
                    .notices
                    .hide_world_writable_warning
                    .unwrap_or(false)
                {
                    let preset = preset.clone();
                    return vec![Box::new(move |tx| {
                        tx.send(AppEvent::CheckWorldWritablePermissionMode {
                            preset: preset.clone(),
                            label: label.clone(),
                            profile_selection: profile_selection.clone(),
                        });
                    })];
                }
            }
        }
        apply_actions()
    }

    pub(crate) async fn apply_permission_mode_after_world_writable_scan(
        &mut self,
        preset: ApprovalPreset,
        label: String,
        profile_selection: Option<PermissionProfileSelection>,
    ) {
        if let Some((sample_paths, extra_count, failed_scan)) =
            self.world_writable_warning_details().await
        {
            self.open_world_writable_warning_confirmation(
                Some(preset),
                profile_selection,
                sample_paths,
                extra_count,
                failed_scan,
            );
            return;
        }
        let actions = profile_selection.map_or_else(
            || {
                Self::approval_preset_actions(
                    AskForApproval::from(preset.approval),
                    preset.permission_profile,
                    preset.active_permission_profile,
                    label,
                )
            },
            Self::permission_profile_selection_actions,
        );
        for action in actions {
            action(&self.app_event_tx);
        }
    }

    pub(super) fn preset_matches_current(
        current_approval: AskForApproval,
        current_permission_profile: &PermissionProfile,
        cwd: &std::path::Path,
        preset: &ApprovalPreset,
    ) -> bool {
        let preset_approval = AskForApproval::from(preset.approval);
        if current_approval != preset_approval {
            return false;
        }

        match preset.id {
            "full-access" => matches!(current_permission_profile, PermissionProfile::Disabled),
            "read-only" => {
                let file_system_policy = current_permission_profile.file_system_sandbox_policy();
                matches!(
                    current_permission_profile,
                    PermissionProfile::Managed { .. }
                ) && !file_system_policy.has_full_disk_write_access()
                    && file_system_policy
                        .get_writable_roots_with_cwd(cwd)
                        .is_empty()
                    && current_permission_profile.network_sandbox_policy()
                        == preset.permission_profile.network_sandbox_policy()
            }
            "auto" => {
                let file_system_policy = current_permission_profile.file_system_sandbox_policy();
                matches!(
                    current_permission_profile,
                    PermissionProfile::Managed { .. }
                ) && file_system_policy.can_write_path_with_cwd(cwd, cwd)
                    && !file_system_policy.has_full_disk_write_access()
                    && current_permission_profile.network_sandbox_policy()
                        == preset.permission_profile.network_sandbox_policy()
            }
            _ => current_permission_profile == &preset.permission_profile,
        }
    }

    pub(crate) fn open_full_access_confirmation(
        &mut self,
        preset: ApprovalPreset,
        return_to_permissions: bool,
        profile_selection: Option<PermissionProfileSelection>,
    ) {
        let selected_name = preset.label.to_string();
        let approval = AskForApproval::from(preset.approval);
        let mut header_children: Vec<Box<dyn Renderable>> = Vec::new();
        let title_line = Line::from("Enable full access?").bold();
        let info_line = Line::from(vec![
            "When Codex runs with full access, it can edit any file on your computer and run commands with network, without your approval. "
                .into(),
            "Exercise caution when enabling full access. This significantly increases the risk of data loss, leaks, or unexpected behavior."
                .fg(Color::Red),
        ]);
        header_children.push(Box::new(title_line));
        header_children.push(Box::new(
            Paragraph::new(vec![info_line]).wrap(Wrap { trim: false }),
        ));
        let header = ColumnRenderable::with(header_children);

        let accept_actions = profile_selection.map_or_else(
            || {
                Self::approval_preset_actions(
                    approval,
                    preset.permission_profile,
                    preset.active_permission_profile,
                    selected_name,
                )
            },
            Self::permission_profile_selection_actions,
        );

        let deny_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            if return_to_permissions {
                tx.send(AppEvent::OpenPermissionsPopup);
            } else {
                tx.send(AppEvent::OpenApprovalsPopup);
            }
        })];

        let items = vec![
            SelectionItem {
                name: "Yes, continue anyway".to_string(),
                description: Some("Apply full access for this session".to_string()),
                actions: accept_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Cancel".to_string(),
                description: Some("Go back without enabling full access".to_string()),
                actions: deny_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(header),
            ..Default::default()
        });
    }
}
