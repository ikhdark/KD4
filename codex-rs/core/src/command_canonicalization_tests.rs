use super::canonicalize_command_for_approval;
use pretty_assertions::assert_eq;

#[test]
fn authorization_identity_preserves_bash_wrapper() {
    let command_a = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "cargo test -p codex-core".to_string(),
    ];
    let command_b = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "cargo   test   -p codex-core".to_string(),
    ];
    let different_path = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "cargo test -p codex-core".to_string(),
    ];
    let different_mode = vec![
        "/bin/bash".to_string(),
        "-c".to_string(),
        "cargo test -p codex-core".to_string(),
    ];
    let different_shell = vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        "cargo test -p codex-core".to_string(),
    ];

    assert_eq!(
        canonicalize_command_for_approval(&command_a),
        vec![
            "__codex_shell_script__".to_string(),
            "/bin/bash".to_string(),
            "-lc".to_string(),
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
        ]
    );
    assert_eq!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&command_b)
    );
    assert_ne!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&different_path)
    );
    assert_ne!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&different_mode)
    );
    assert_ne!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&different_shell)
    );
}

#[test]
fn preserves_heredoc_wrapper_identity() {
    let script = "python3 <<'PY'\nprint('hello')\nPY";
    let command_a = vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        script.to_string(),
    ];
    let command_b = vec!["zsh".to_string(), "-lc".to_string(), script.to_string()];

    assert_eq!(
        canonicalize_command_for_approval(&command_a),
        vec![
            "__codex_shell_script__".to_string(),
            "/bin/zsh".to_string(),
            "-lc".to_string(),
            script.to_string(),
        ]
    );
    assert_ne!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&command_b)
    );
}

#[test]
fn authorization_identity_separates_bash_quote_semantics() {
    let double_quoted = vec![
        "bash".to_string(),
        "-lc".to_string(),
        r#"tool "a\"b""#.to_string(),
    ];
    let single_quoted = vec![
        "bash".to_string(),
        "-lc".to_string(),
        r#"tool 'a\"b'"#.to_string(),
    ];

    assert_ne!(
        canonicalize_command_for_approval(&double_quoted),
        canonicalize_command_for_approval(&single_quoted)
    );
}

#[test]
fn canonicalizes_powershell_wrappers_without_crossing_profile_modes() {
    let script = "Write-Host hi";
    let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
        .to_string_lossy()
        .into_owned();
    let command_a = vec![
        powershell.clone(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ];
    let command_b = vec![powershell.clone(), "-Command".to_string(), script.to_string()];

    assert_eq!(
        canonicalize_command_for_approval(&command_a),
        vec![
            "__codex_powershell_script__".to_string(),
            powershell.clone(),
            "no-profile".to_string(),
            script.to_string(),
        ]
    );
    assert_eq!(
        canonicalize_command_for_approval(&command_b),
        vec![
            "__codex_powershell_script__".to_string(),
            powershell,
            "profiles-enabled".to_string(),
            script.to_string(),
        ]
    );
    assert_ne!(
        canonicalize_command_for_approval(&command_a),
        canonicalize_command_for_approval(&command_b)
    );
}

#[test]
fn powershell_approval_identity_preserves_host_and_execution_flags() {
    let windows_powershell =
        std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe")
            .to_string_lossy()
            .into_owned();
    let mut hosts = vec![windows_powershell];
    if let Some(pwsh) = codex_shell_command::powershell::try_find_pwsh_executable_blocking() {
        hosts.push(pwsh.as_path().to_string_lossy().into_owned());
    }
    let mut identities = std::collections::HashSet::new();
    for host in hosts {
        for flags in [
            vec![],
            vec!["-NoProfile"],
            vec!["-NonInteractive"],
            vec!["-STA"],
            vec!["-MTA"],
            vec!["-NoLogo"],
            vec!["-STA", "-MTA"],
            vec!["-MTA", "-STA"],
            vec!["-NoProfile", "-NonInteractive", "-STA"],
        ] {
            let mut command = vec![host.clone()];
            command.extend(flags.iter().map(|flag| (*flag).to_string()));
            command.extend(["-Command".to_string(), "Write-Host Hi".to_string()]);
            let expected = canonicalize_command_for_approval(&command);
            assert_eq!(expected[0], "__codex_powershell_script__");
            assert!(identities.insert(expected.clone()), "{command:?}");

            for introducer in ["-c", "/COMMAND", "-Command:", "/cOmMaNd:"] {
                let mut alias = vec![host.clone()];
                alias.extend(flags.iter().map(|flag| flag.to_ascii_lowercase()));
                if introducer.ends_with(':') {
                    alias.push(format!("{introducer}Write-Host Hi"));
                } else {
                    alias.extend([introducer.to_string(), "Write-Host Hi".to_string()]);
                }
                assert_eq!(canonicalize_command_for_approval(&alias), expected);
            }
            command.last_mut().expect("script").push(' ');
            assert_ne!(canonicalize_command_for_approval(&command), expected);
        }
    }
}

#[test]
fn preserves_non_shell_commands() {
    let command = vec!["cargo".to_string(), "fmt".to_string()];
    assert_eq!(canonicalize_command_for_approval(&command), command);
}

#[test]
fn preserves_untrusted_powershell_wrapper_identity() {
    let command = vec![
        "./workspace-local/pwsh.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "Get-ChildItem".to_string(),
    ];

    assert_eq!(canonicalize_command_for_approval(&command), command);
}
