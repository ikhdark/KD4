//! Real ToolCallRuntime -> registered handler -> runtime -> native child path.
//! Cargo/Just stand-ins write a marker instead of compiling anything. The
//! positive controls prove that rejection is not merely a broken spawn fixture.
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ExecCommandHandler;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use codex_exec_server::Environment;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn executable(directory: &Path, name: &str, managed: bool) -> PathBuf {
    let path = directory.join(if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    });
    let source = if cfg!(windows) {
        if managed {
            "@\"%KD4_CARGO_FIXTURE%\" build\r\n"
        } else {
            "@echo off\r\n> \"%KD4_CARGO_ADMISSION_MARKER%\" echo spawned:%CARGO_TARGET_DIR%\r\n"
        }
    } else if managed {
        "#!/bin/sh\nexec \"$KD4_CARGO_FIXTURE\" build\n"
    } else {
        "#!/bin/sh\nprintf 'spawned:%s\\n' \"$CARGO_TARGET_DIR\" > \"$KD4_CARGO_ADMISSION_MARKER\"\n"
    };
    std::fs::write(&path, source).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

async fn exercise_dispatch(tool: &str) {
    for (case, args, target, build, include, managed, script, allowed) in [
        (
            "generic-default",
            vec!["test", "--locked"],
            "",
            None,
            false,
            false,
            false,
            true,
        ),
        (
            "generic-cli-config",
            vec!["check", "--config", "build.target-dir='ordinary-output'"],
            "",
            None,
            false,
            false,
            false,
            true,
        ),
        (
            "generic-managed-target",
            vec!["build"],
            "reserved/debug",
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "generic-managed-build",
            vec!["build", "--target-dir=ordinary-output"],
            "ordinary-output",
            Some("reserved/intermediates"),
            false,
            false,
            false,
            false,
        ),
        (
            "generic-shell-control",
            vec!["build"],
            "ordinary-output",
            None,
            false,
            false,
            true,
            true,
        ),
        (
            "generic-shell-env-newline",
            vec!["build"],
            "",
            None,
            false,
            false,
            true,
            false,
        ),
        (
            "generic-shell-env-crlf",
            vec!["build"],
            "",
            None,
            false,
            false,
            true,
            false,
        ),
        (
            "generic-shell-env-try",
            vec!["build"],
            "",
            None,
            false,
            false,
            true,
            false,
        ),
        (
            "plugin-unrelated",
            vec!["custom-plugin", "--target-dir=target/unrelated"],
            "target/lanes/core-tests",
            None,
            false,
            false,
            false,
            true,
        ),
        (
            "env-target",
            vec!["build"],
            "target/lanes/core-tests",
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "env-intermediates",
            vec!["custom-plugin", "--target-dir=target/unrelated"],
            "target/unrelated",
            Some("target/lanes/core-tests"),
            false,
            false,
            false,
            false,
        ),
        (
            "included-intermediates",
            vec!["build", "--target-dir=target/unrelated"],
            "target/unrelated",
            None,
            true,
            false,
            false,
            false,
        ),
        (
            "shell-env-target",
            vec!["build"],
            "target/lanes/core-tests",
            None,
            false,
            false,
            true,
            false,
        ),
        (
            "configured-alias",
            vec!["fixturebuild"],
            "target/unrelated",
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "inspection",
            vec!["metadata", "--no-deps"],
            "target/lanes/core-tests",
            None,
            false,
            false,
            false,
            true,
        ),
        (
            "managed-child",
            vec!["core-test-fast", "core_lib", "-E", "test(=fixture)"],
            "target/lanes/core-tests",
            None,
            false,
            true,
            false,
            true,
        ),
    ] {
        let powershell_boundary = case.starts_with("generic-shell-");
        if powershell_boundary && !cfg!(windows) {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let generic = case.starts_with("generic-");
        let cwd = root.join(if generic {
            "ordinary-project"
        } else {
            "codex-rs"
        });
        if generic {
            std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
            std::fs::write(
                cwd.join(".cargo/config.toml"),
                "[env]\nRUST_TEST_THREADS={value='8',force=false}\n",
            )
            .unwrap();
            std::fs::write(
                cwd.join("Cargo.toml"),
                "[package]\nname='ordinary-project'\nversion='0.1.0'\n",
            )
            .unwrap();
            if !allowed {
                std::fs::create_dir_all(cwd.join("reserved")).unwrap();
                std::fs::write(
                    cwd.join("reserved/.codex-cargo-lanes-root"),
                    "codex-kd cargo lanes root v1\n",
                )
                .unwrap();
            }
        } else {
            std::fs::create_dir_all(cwd.join(".config")).unwrap();
            std::fs::write(cwd.join(".config/kd4-rust-tests.toml"), "").unwrap();
        }
        if include {
            std::fs::create_dir_all(cwd.join(".cargo/nested")).unwrap();
            std::fs::write(
                cwd.join(".cargo/config.toml"),
                "include=['nested/protected.toml']\n",
            )
            .unwrap();
            std::fs::write(
                cwd.join(".cargo/nested/protected.toml"),
                "[build]\nbuild-dir='../target/lanes/core-tests'\n",
            )
            .unwrap();
        }
        if case == "configured-alias" {
            std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
            std::fs::write(
                cwd.join(".cargo/config.toml"),
                "[alias]\nfixturebuild='build --target-dir target/lanes/core-tests'\n",
            )
            .unwrap();
        }
        let marker = root.join("spawned.txt");
        let cargo = executable(root, "cargo", false);
        let program = if managed {
            executable(root, "just", true)
        } else {
            cargo.clone()
        };
        let absolute_cwd = AbsolutePathBuf::try_from(cwd.clone()).unwrap();
        let cwd_uri = PathUri::from_abs_path(&absolute_cwd);
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.approval_policy.set(AskForApproval::Never).unwrap();
        turn.permission_profile = PermissionProfile::Disabled;
        let config = Arc::make_mut(&mut turn.config);
        config.cwd = absolute_cwd;
        config
            .features
            .enable(codex_features::Feature::Kd4Runtime)
            .unwrap();
        let policy = &mut config.permissions.shell_environment_policy;
        policy.inherit = ShellEnvironmentPolicyInherit::None;
        policy.r#set = std::env::vars()
            .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("CARGO_"))
            .collect();
        policy.r#set.extend([
            (
                "CARGO_HOME".into(),
                root.join("cargo-home").display().to_string(),
            ),
            ("KD4_CARGO_FIXTURE".into(), cargo.display().to_string()),
            (
                "KD4_CARGO_ADMISSION_MARKER".into(),
                marker.display().to_string(),
            ),
        ]);
        if powershell_boundary {
            // Bare Cargo is essential: an `&` call operator would hide the
            // lost-newline/block-boundary regression. Resolve only our stand-in
            // first, with an allowed dispatch proving that it can be launched.
            let inherited_path = std::env::var_os("PATH").unwrap_or_default();
            let path = std::env::join_paths(
                std::iter::once(root.to_path_buf()).chain(std::env::split_paths(&inherited_path)),
            )
            .unwrap();
            policy.r#set.retain(|key, _| !key.eq_ignore_ascii_case("PATH"));
            policy.r#set.insert("PATH".into(), path.into_string().unwrap());
        }
        if !target.is_empty() {
            policy
                .r#set
                .insert("CARGO_TARGET_DIR".into(), target.into());
        }
        if let Some(build) = build {
            policy
                .r#set
                .insert("CARGO_BUILD_BUILD_DIR".into(), build.into());
        }
        // Use native paths so shell_command exercises its own runtime rather
        // than the foreign-environment forwarding path to unified exec.
        turn.environments.turn_environments = vec![TurnEnvironment::new(
            LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(Environment::default_for_tests()),
            cwd_uri,
            None,
        )];
        let handler: Arc<dyn CoreToolRuntime> = if tool == "exec_command" {
            Arc::new(ExecCommandHandler::default())
        } else {
            Arc::new(ShellCommandHandler::default())
        };
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
        let dispatch = ToolCallRuntime::new(
            Arc::new(session),
            step,
            Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
        );
        let arguments = if script {
            let command = format!(
                "{}'{}' {}",
                if cfg!(windows) { "& " } else { "" },
                program
                    .to_string_lossy()
                    .replace('\'', if cfg!(windows) { "''" } else { "'\\''" }),
                args.join(" ")
            );
            let command = match case {
                "generic-shell-control" => "cargo build".to_string(),
                "generic-shell-env-newline" => {
                    "$env:CARGO_TARGET_DIR='reserved'\ncargo build".to_string()
                }
                "generic-shell-env-crlf" => {
                    "$env:CARGO_TARGET_DIR='reserved'\r\ncargo build".to_string()
                }
                "generic-shell-env-try" => {
                    "$env:CARGO_TARGET_DIR='reserved'; try { cargo build } finally {}".to_string()
                }
                _ => command,
            };
            // Default handlers do not advertise `login`; omitting it already
            // selects a non-login shell.
            if cfg!(windows) {
                serde_json::json!({"script_body": command, "workdir": cwd})
            } else if tool == "exec_command" {
                serde_json::json!({"cmd": command, "workdir": cwd})
            } else {
                serde_json::json!({"command": command, "workdir": cwd})
            }
        } else {
            serde_json::json!({"program": program, "args": args, "workdir": cwd})
        };
        let response = tokio::time::timeout(
            Duration::from_secs(20),
            dispatch.handle_tool_call(
                ToolCall {
                    tool_name: codex_tools::ToolName::plain(tool),
                    call_id: format!("cargo-lane-{case}"),
                    payload: ToolPayload::Function {
                        arguments: arguments.to_string(),
                    },
                },
                CancellationToken::new(),
            ),
        )
        .await
        .expect("dispatch must settle")
        .expect("tool response");
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            panic!("{tool}/{case}: expected a function response");
        };
        let FunctionCallOutputBody::Text(message) = output.body else {
            panic!("{tool}/{case}: expected text");
        };
        if allowed {
            assert_ne!(output.success, Some(false), "{tool}/{case}: {message}");
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap().trim(),
                format!("spawned:{target}"),
                "{tool}/{case}: {message}"
            );
        } else {
            assert_eq!(output.success, Some(false), "{tool}/{case}: {message}");
            assert!(
                message.contains("Raw Cargo launch rejected; no build started"),
                "{tool}/{case}: {message}"
            );
            assert!(
                !marker.exists(),
                "{tool}/{case}: rejected child was launched"
            );
        }
    }
}

#[tokio::test]
async fn unified_exec_dispatch_contains_effective_cargo_directories() {
    exercise_dispatch("exec_command").await;
}

#[tokio::test]
async fn shell_dispatch_contains_effective_cargo_directories() {
    exercise_dispatch("shell_command").await;
}
