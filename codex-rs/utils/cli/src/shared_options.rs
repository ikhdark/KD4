//! Shared command-line flags used by both interactive and non-interactive Codex entry points.

use crate::SandboxModeCliArg;
use clap::Args;
use codex_protocol::config_types::ProfileV2Name;
use std::path::PathBuf;

#[derive(Args, Clone, Debug, Default)]
pub struct SharedCliOptions {
    /// Optional image(s) to attach to the initial prompt.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub images: Vec<PathBuf>,

    /// Model the agent should use.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Use open-source provider.
    #[arg(long = "oss", default_value_t = false)]
    pub oss: bool,

    /// Specify which local provider to use (lmstudio or ollama).
    /// If not specified with --oss, will use config default or show selection.
    #[arg(long = "local-provider", requires = "oss")]
    pub oss_provider: Option<String>,

    /// Layer $CODEX_HOME/<name>.config.toml on top of the base user config.
    #[arg(long = "profile", short = 'p')]
    pub config_profile_v2: Option<ProfileV2Name>,

    /// Select the sandbox policy to use when executing model-generated shell
    /// commands.
    #[arg(long = "sandbox", short = 's')]
    pub sandbox_mode: Option<SandboxModeCliArg>,

    /// Skip all confirmation prompts and execute commands without sandboxing.
    /// EXTREMELY DANGEROUS. Intended solely for running in environments that are externally sandboxed.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Run enabled hooks without requiring persisted hook trust for this invocation.
    /// DANGEROUS. Intended only for automation that already vets hook sources.
    #[arg(long = "dangerously-bypass-hook-trust", default_value_t = false)]
    pub bypass_hook_trust: bool,

    /// Tell the agent to use the specified directory as its working root.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Additional directories that should be writable alongside the primary workspace.
    #[arg(long = "add-dir", value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    pub add_dir: Vec<PathBuf>,
}

impl SharedCliOptions {
    /// Merges these subcommand options over `root`, as
    /// [`Self::apply_subcommand_overrides`] does for resumed sessions.
    pub fn inherit_exec_root_options(&mut self, root: &Self) {
        let subcommand = std::mem::replace(self, root.clone());
        self.apply_subcommand_overrides(subcommand);
    }

    /// Layers options given after a subcommand over these root options.
    ///
    /// Values set on the subcommand win, the sandbox selection (`--sandbox` or
    /// `--dangerously-bypass-approvals-and-sandbox`) is replaced as a unit,
    /// flags stay enabled if either level set them, and lists append the
    /// subcommand's entries after the root's.
    pub fn apply_subcommand_overrides(&mut self, subcommand: Self) {
        let subcommand_selected_sandbox_mode = subcommand.sandbox_mode.is_some()
            || subcommand.dangerously_bypass_approvals_and_sandbox;
        let Self {
            images,
            model,
            oss,
            oss_provider,
            config_profile_v2,
            sandbox_mode,
            dangerously_bypass_approvals_and_sandbox,
            bypass_hook_trust,
            cwd,
            add_dir,
        } = subcommand;

        if let Some(model) = model {
            self.model = Some(model);
        }
        if oss {
            self.oss = true;
        }
        if let Some(oss_provider) = oss_provider {
            self.oss_provider = Some(oss_provider);
        }
        if let Some(config_profile_v2) = config_profile_v2 {
            self.config_profile_v2 = Some(config_profile_v2);
        }
        if subcommand_selected_sandbox_mode {
            self.sandbox_mode = sandbox_mode;
            self.dangerously_bypass_approvals_and_sandbox =
                dangerously_bypass_approvals_and_sandbox;
        }
        if bypass_hook_trust {
            self.bypass_hook_trust = true;
        }
        if let Some(cwd) = cwd {
            self.cwd = Some(cwd);
        }
        if !images.is_empty() {
            self.images.extend(images);
        }
        if !add_dir.is_empty() {
            self.add_dir.extend(add_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        shared: SharedCliOptions,
    }

    #[test]
    fn inherits_sandbox_selection_only_when_child_has_no_selection() {
        for (root_args, child_args, expected_mode, expected_bypass) in [
            (
                vec!["codex", "--sandbox", "read-only"],
                vec!["codex", "--yolo"],
                None,
                true,
            ),
            (
                vec!["codex", "--yolo"],
                vec!["codex", "--sandbox", "read-only"],
                Some(SandboxModeCliArg::ReadOnly),
                false,
            ),
            (
                vec!["codex", "--sandbox", "read-only"],
                vec!["codex"],
                Some(SandboxModeCliArg::ReadOnly),
                false,
            ),
            (vec!["codex", "--yolo"], vec!["codex"], None, true),
        ] {
            let root = Cli::try_parse_from(root_args).expect("root args").shared;
            let mut child = Cli::try_parse_from(child_args).expect("child args").shared;
            child.inherit_exec_root_options(&root);
            assert_eq!(
                child
                    .sandbox_mode
                    .map(codex_protocol::config_types::SandboxMode::from),
                expected_mode.map(codex_protocol::config_types::SandboxMode::from)
            );
            assert_eq!(
                child.dangerously_bypass_approvals_and_sandbox,
                expected_bypass
            );
        }
    }

    #[test]
    fn root_and_subcommand_images_merge_in_both_directions() {
        let root = Cli::try_parse_from(["codex", "-i", "root.png"])
            .expect("root args")
            .shared;
        let child = Cli::try_parse_from(["codex", "-i", "child.png"])
            .expect("child args")
            .shared;

        let mut exec_child = child.clone();
        exec_child.inherit_exec_root_options(&root);
        let mut resumed = root.clone();
        resumed.apply_subcommand_overrides(child);

        let expected = vec![PathBuf::from("root.png"), PathBuf::from("child.png")];
        assert_eq!(exec_child.images, expected);
        assert_eq!(resumed.images, expected);
    }

    #[test]
    fn exec_child_values_win_and_root_values_fill_gaps() {
        let root = Cli::try_parse_from([
            "codex",
            "-m",
            "root-model",
            "-C",
            "root-dir",
            "--add-dir",
            "root-extra",
            "--oss",
            "--local-provider",
            "ollama",
        ])
        .expect("root args")
        .shared;
        let mut child = Cli::try_parse_from([
            "codex",
            "-m",
            "child-model",
            "--add-dir",
            "child-extra",
            "--dangerously-bypass-hook-trust",
        ])
        .expect("child args")
        .shared;

        child.inherit_exec_root_options(&root);

        assert_eq!(
            (
                child.model.as_deref(),
                child.cwd,
                child.add_dir,
                child.oss,
                child.oss_provider.as_deref(),
                child.bypass_hook_trust,
            ),
            (
                Some("child-model"),
                Some(PathBuf::from("root-dir")),
                vec![PathBuf::from("root-extra"), PathBuf::from("child-extra")],
                true,
                Some("ollama"),
                true,
            )
        );
    }
}
