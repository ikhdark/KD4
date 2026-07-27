use super::KDWIREGUARD_DISPLAY_LABEL;
use super::for_command;

fn command(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

#[test]
fn labels_direct_wrapper_argv_without_using_an_install_path() {
    let command = command(&[
        r"D:\Portable Tools\KDS\kds.exe",
        "--agent",
        "--",
        "cargo",
        "test",
    ]);

    assert_eq!(
        for_command(&command).as_deref(),
        Some(KDWIREGUARD_DISPLAY_LABEL)
    );
}

#[test]
fn labels_windows_powershell_and_cmd_quoting_for_different_users() {
    let cases = [
        command(&[
            "powershell.exe",
            "-NoProfile",
            "-Command",
            r"& 'C:\Users\Alice Example\AppData\Local\plugins\kds.exe' --agent -- cargo check",
        ]),
        command(&[
            "pwsh",
            "-Command",
            r#"& "E:\Company Apps\private\KDS.EXE" "--agent" -- cargo test"#,
        ]),
        command(&[
            "cmd.exe",
            "/d",
            "/s",
            "/c",
            r#""C:\Users\Bob\Plugin Cache\kds.exe" --agent -- cargo build"#,
        ]),
        command(&[
            "cmd.exe",
            "/d",
            "/s",
            "/c",
            r#"""C:\Users\Charlie\Private Install\kds.exe" --agent -- cargo build""#,
        ]),
        command(&[
            "cmd",
            "/c",
            r#"call "Z:\Codex Plugins\kds" --agent -- npm test"#,
        ]),
    ];

    for command in cases {
        assert_eq!(
            for_command(&command).as_deref(),
            Some(KDWIREGUARD_DISPLAY_LABEL),
            "{command:?}"
        );
    }
}

#[test]
fn labels_posix_shells_and_install_roots() {
    let cases = [
        command(&[
            "/bin/bash",
            "-lc",
            "'/home/alice/.cache/codex plugins/kds' --agent -- cargo test",
        ]),
        command(&[
            "/bin/zsh",
            "-c",
            "exec '/Users/bob/Library/Application Support/Codex/kds' --agent -- make test",
        ]),
        command(&[
            "/usr/bin/fish",
            "-c",
            "command '/opt/company tools/kds' --agent -- pytest",
        ]),
        command(&[
            "/usr/local/bin/nu",
            "-c",
            "'/srv/codex/kds' --agent -- go test ./...",
        ]),
        command(&[
            "/bin/bash",
            "-lc",
            r"/opt/company\ tools/kds --agent -- cargo test",
        ]),
    ];

    for command in cases {
        assert_eq!(
            for_command(&command).as_deref(),
            Some(KDWIREGUARD_DISPLAY_LABEL),
            "{command:?}"
        );
    }
}

#[test]
fn labels_only_an_executed_wrapper_with_the_agent_marker() {
    let cases = [
        command(&["kds.exe", "--", "cargo", "test"]),
        command(&["kds-helper", "--agent", "--", "cargo", "test"]),
        command(&["echo", "kds", "--agent"]),
        command(&[
            "powershell.exe",
            "-Command",
            r#"Write-Host "C:\Users\Alice\kds.exe --agent""#,
        ]),
        command(&["/bin/bash", "-lc", "echo '/home/alice/cache/kds --agent'"]),
        command(&["python", "-c", "print('kds --agent')"]),
    ];

    for command in cases {
        assert_eq!(for_command(&command), None, "{command:?}");
    }
}
