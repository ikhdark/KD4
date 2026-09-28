use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

use super::CODEX_HOME_FLAG;
use super::CODEX_WINDOWS_SANDBOX_ARG1;
use super::COMMAND_CWD_FLAG;
use super::DENY_READ_PATHS_JSON_FLAG;
use super::DENY_WRITE_PATHS_JSON_FLAG;
use super::ENV_JSON_FLAG;
use super::PERMISSION_PROFILE_FLAG;
use super::PRESERVE_PROXY_SETTINGS_FLAG;
use super::PRIVATE_DESKTOP_FLAG;
use super::PROXY_ENFORCED_FLAG;
use super::READ_ROOTS_INCLUDE_PLATFORM_DEFAULTS_FLAG;
use super::READ_ROOTS_JSON_FLAG;
use super::SANDBOX_LEVEL_FLAG;
use super::WORKSPACE_ROOT_FLAG;
use super::WRITE_ROOTS_JSON_FLAG;
use super::create_windows_sandbox_command_args_for_permission_profile;
use super::parse_windows_sandbox_wrapper_args;

#[test]
fn windows_wrapper_args_round_trip() {
    let command_cwd = AbsolutePathBuf::from_absolute_path(Path::new(r"C:\workspace"))
        .expect("absolute command cwd");
    let workspace_roots = vec![
        command_cwd.clone(),
        AbsolutePathBuf::from_absolute_path(Path::new(r"D:\other-workspace"))
            .expect("absolute workspace root"),
    ];
    let mut env = HashMap::from([("Path".to_string(), r"C:\Windows\System32".to_string())]);
    let permission_profile = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };
    let read_roots_override = vec![PathBuf::from(r"C:\read")];
    let write_roots_override = vec![PathBuf::from(r"C:\write")];
    let deny_read_paths_override = vec![
        AbsolutePathBuf::from_absolute_path(Path::new(r"C:\blocked-read"))
            .expect("absolute deny-read"),
    ];
    let deny_write_paths_override = vec![
        AbsolutePathBuf::from_absolute_path(Path::new(r"C:\blocked-write"))
            .expect("absolute deny-write"),
    ];

    let args = create_windows_sandbox_command_args_for_permission_profile(
        vec![
            "codex.exe".to_string(),
            "--codex-run-as-fs-helper".to_string(),
        ],
        &command_cwd,
        workspace_roots.as_slice(),
        &mut env,
        &permission_profile,
        WindowsSandboxLevel::Elevated,
        /*windows_sandbox_private_desktop*/ true,
        /*proxy_enforced*/ true,
        crate::WindowsSandboxProxySettingsMode::Preserve,
        Some(read_roots_override.as_slice()),
        /*read_roots_include_platform_defaults*/ true,
        Some(write_roots_override.as_slice()),
        deny_read_paths_override.as_slice(),
        deny_write_paths_override.as_slice(),
        Path::new(r"C:\Users\me\.codex"),
    )
    .expect("prepare wrapper args");

    assert_eq!(args[0], CODEX_WINDOWS_SANDBOX_ARG1);
    assert!(args.contains(&CODEX_HOME_FLAG.to_string()));
    assert!(args.contains(&COMMAND_CWD_FLAG.to_string()));
    assert!(args.contains(&WORKSPACE_ROOT_FLAG.to_string()));
    assert!(args.contains(&PERMISSION_PROFILE_FLAG.to_string()));
    assert!(args.contains(&ENV_JSON_FLAG.to_string()));
    assert!(args.contains(&SANDBOX_LEVEL_FLAG.to_string()));
    assert!(args.contains(&PRIVATE_DESKTOP_FLAG.to_string()));
    assert!(args.contains(&PROXY_ENFORCED_FLAG.to_string()));
    assert!(args.contains(&PRESERVE_PROXY_SETTINGS_FLAG.to_string()));
    assert!(args.contains(&READ_ROOTS_JSON_FLAG.to_string()));
    assert!(args.contains(&READ_ROOTS_INCLUDE_PLATFORM_DEFAULTS_FLAG.to_string()));
    assert!(args.contains(&WRITE_ROOTS_JSON_FLAG.to_string()));
    assert!(args.contains(&DENY_READ_PATHS_JSON_FLAG.to_string()));
    assert!(args.contains(&DENY_WRITE_PATHS_JSON_FLAG.to_string()));

    let parsed =
        parse_windows_sandbox_wrapper_args(args[1..].to_vec()).expect("parse wrapper args");

    assert_eq!(
        parsed.command,
        vec!["codex.exe", "--codex-run-as-fs-helper"]
    );
    assert_eq!(parsed.command_cwd, command_cwd);
    assert_eq!(parsed.workspace_roots, workspace_roots);
    assert_eq!(parsed.env_map, env);
    assert_eq!(parsed.permission_profile, permission_profile);
    assert_eq!(parsed.windows_sandbox_level, WindowsSandboxLevel::Elevated);
    assert_eq!(parsed.windows_sandbox_private_desktop, true);
    assert_eq!(parsed.proxy_enforced, true);
    assert_eq!(
        parsed.proxy_settings_mode,
        crate::WindowsSandboxProxySettingsMode::Preserve
    );
    assert_eq!(parsed.read_roots_override, Some(read_roots_override));
    assert_eq!(parsed.read_roots_include_platform_defaults, true);
    assert_eq!(parsed.write_roots_override, Some(write_roots_override));
    assert_eq!(parsed.deny_read_paths_override, deny_read_paths_override);
    assert_eq!(parsed.deny_write_paths_override, deny_write_paths_override);
}

#[test]
fn large_wrapper_payload_uses_child_environment_without_changing_inner_env() {
    use std::os::windows::process::CommandExt;
    let cwd = AbsolutePathBuf::from_absolute_path(Path::new(r"C:\fixture")).unwrap();
    let mut env = HashMap::from([("Path".to_string(), "selected-toolchain".to_string())]);
    let denied: Vec<_> = (0..1000)
        .map(|i| {
            AbsolutePathBuf::from_absolute_path(PathBuf::from(format!(
                "C:\\fixture\\project-{i}\\配置.secret"
            )))
            .unwrap()
        })
        .collect();
    let args = create_windows_sandbox_command_args_for_permission_profile(
        vec!["cmd.exe".into(), "/c".into(), "echo hello".into()],
        &cwd,
        &[],
        &mut env,
        &PermissionProfile::External {
            network: NetworkSandboxPolicy::Restricted,
        },
        WindowsSandboxLevel::Elevated,
        false,
        false,
        crate::WindowsSandboxProxySettingsMode::Preserve,
        None,
        false,
        None,
        &denied,
        &[],
        Path::new(r"C:\fixture\home"),
    )
    .unwrap();
    assert!(args.iter().map(String::len).sum::<usize>() < 1000);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "wrapper::tests::large_wrapper_payload_child",
            "--nocapture",
        ])
        .envs(&env)
        .env(
            "CODEX_TEST_WRAPPER_ARGS",
            serde_json::to_string(&args[1..]).unwrap(),
        )
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("verified wrapper policy and isolated inner env")
    );
    assert!(super::decode_wrapper_args(args[1..].to_vec(), |_| None).is_err());
    assert!(
        super::decode_wrapper_args(
            vec![
                super::ARGS_ENV_FLAG.into(),
                super::ARGS_ENV_PREFIX.into(),
                usize::MAX.to_string()
            ],
            |_| None
        )
        .is_err()
    );
}

#[test]
fn large_wrapper_payload_child() {
    let Ok(args) = std::env::var("CODEX_TEST_WRAPPER_ARGS") else {
        return;
    };
    let parsed = parse_windows_sandbox_wrapper_args(serde_json::from_str(&args).unwrap()).unwrap();
    assert_eq!(
        parsed.env_map,
        HashMap::from([("Path".to_string(), "selected-toolchain".to_string())])
    );
    assert_eq!(parsed.command, vec!["cmd.exe", "/c", "echo hello"]);
    assert_eq!(parsed.deny_read_paths_override.len(), 1000);
    for (i, path) in parsed.deny_read_paths_override.iter().enumerate() {
        assert_eq!(
            path.as_path(),
            Path::new(&format!("C:\\fixture\\project-{i}\\配置.secret"))
        );
    }
    println!("verified wrapper policy and isolated inner env");
}
