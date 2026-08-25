use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum ShellType {
    Zsh,
    Bash,
    PowerShell,
    Sh,
    Cmd,
}

impl ShellType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::PowerShell => "powershell",
            Self::Sh => "sh",
            Self::Cmd => "cmd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedShell {
    pub shell_type: ShellType,
    pub shell_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerShellHostKind {
    Pwsh,
    WindowsPowerShell,
}

impl DetectedShell {
    pub fn name(&self) -> &'static str {
        self.shell_type.name()
    }

    pub fn powershell_host_kind(&self) -> Option<PowerShellHostKind> {
        (self.shell_type == ShellType::PowerShell)
            .then(|| powershell_host_kind(&self.shell_path))
            .flatten()
    }
}

pub fn powershell_host_kind(path: impl AsRef<std::path::Path>) -> Option<PowerShellHostKind> {
    match path
        .as_ref()
        .file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pwsh") => Some(PowerShellHostKind::Pwsh),
        Some("powershell") => Some(PowerShellHostKind::WindowsPowerShell),
        _ => None,
    }
}

pub fn detect_shell_type(shell_path: impl AsRef<std::path::Path>) -> Option<ShellType> {
    let shell_path = shell_path.as_ref();
    match shell_path
        .as_os_str()
        .to_str()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("zsh") => Some(ShellType::Zsh),
        Some("bash") => Some(ShellType::Bash),
        Some("sh") => Some(ShellType::Sh),
        Some("cmd") => Some(ShellType::Cmd),
        Some("pwsh") => Some(ShellType::PowerShell),
        Some("powershell") => Some(ShellType::PowerShell),
        _ => {
            let shell_name = shell_path.file_stem();
            if let Some(shell_name) = shell_name {
                let shell_name_path = std::path::Path::new(shell_name);
                if shell_name_path != shell_path {
                    return detect_shell_type(shell_name_path);
                }
            }
            None
        }
    }
}

fn get_user_shell_path() -> Option<PathBuf> {
    None
}

fn file_exists(path: &std::path::Path) -> Option<PathBuf> {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        Some(PathBuf::from(path))
    } else {
        None
    }
}

fn get_shell_path(
    shell_type: ShellType,
    provided_path: Option<&PathBuf>,
    binary_name: &str,
    fallback_paths: &[&str],
) -> Option<PathBuf> {
    if let Some(path) = provided_path.and_then(|path| file_exists(path)) {
        return Some(path);
    }

    let default_shell_path = get_user_shell_path();
    if let Some(default_shell_path) = default_shell_path
        && detect_shell_type(&default_shell_path) == Some(shell_type)
        && file_exists(&default_shell_path).is_some()
    {
        return Some(default_shell_path);
    }

    if let Ok(path) = which::which(binary_name) {
        return Some(path);
    }

    for path in fallback_paths {
        if let Some(path) = file_exists(std::path::Path::new(path)) {
            return Some(path);
        }
    }

    None
}

// Note the `pwsh` and `powershell` fallback paths are where the respective
// shells are commonly installed on GitHub Actions Windows runners, but may not
// be present on all Windows machines:
// https://docs.github.com/en/actions/tutorials/build-and-test-code/powershell

const PWSH_FALLBACK_PATHS: &[&str] = &[r#"C:\Program Files\PowerShell\7\pwsh.exe"#];

const POWERSHELL_FALLBACK_PATHS: &[&str] =
    &[r#"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"#];

fn get_powershell_shell(path: Option<&PathBuf>) -> Option<DetectedShell> {
    let provided = path.and_then(|path| file_exists(path));
    let pwsh = find_binary_or_fallback("pwsh", PWSH_FALLBACK_PATHS);
    let default_powershell = get_user_shell_path().and_then(|default_shell_path| {
        (detect_shell_type(&default_shell_path) == Some(ShellType::PowerShell))
            .then(|| file_exists(&default_shell_path))
            .flatten()
    });
    let windows_powershell = find_binary_or_fallback("powershell", POWERSHELL_FALLBACK_PATHS);
    let shell_path = select_powershell_host(provided, pwsh, default_powershell, windows_powershell);

    shell_path.map(|shell_path| DetectedShell {
        shell_type: ShellType::PowerShell,
        shell_path,
    })
}

fn find_binary_or_fallback(binary_name: &str, fallback_paths: &[&str]) -> Option<PathBuf> {
    which::which(binary_name).ok().or_else(|| {
        fallback_paths
            .iter()
            .find_map(|path| file_exists(std::path::Path::new(path)))
    })
}

fn select_powershell_host(
    provided: Option<PathBuf>,
    pwsh: Option<PathBuf>,
    default_powershell: Option<PathBuf>,
    windows_powershell: Option<PathBuf>,
) -> Option<PathBuf> {
    provided
        .or(pwsh)
        .or(default_powershell)
        .or(windows_powershell)
}

fn get_cmd_shell(path: Option<&PathBuf>) -> Option<DetectedShell> {
    let shell_path = get_shell_path(ShellType::Cmd, path, "cmd", &[]);

    shell_path.map(|shell_path| DetectedShell {
        shell_type: ShellType::Cmd,
        shell_path,
    })
}

pub fn ultimate_fallback_shell() -> DetectedShell {
    DetectedShell {
        shell_type: ShellType::Cmd,
        shell_path: PathBuf::from("cmd.exe"),
    }
}

pub fn get_shell_by_model_provided_path(shell_path: &PathBuf) -> Option<DetectedShell> {
    detect_shell_type(shell_path).and_then(|shell_type| get_shell(shell_type, Some(shell_path)))
}

pub fn get_shell(shell_type: ShellType, path: Option<&PathBuf>) -> Option<DetectedShell> {
    match shell_type {
        ShellType::PowerShell => get_powershell_shell(path),
        ShellType::Cmd => get_cmd_shell(path),
        ShellType::Zsh | ShellType::Bash | ShellType::Sh => None,
    }
}

pub fn default_user_shell() -> DetectedShell {
    default_user_shell_from_path(get_user_shell_path())
}

pub fn default_user_shell_from_path(_user_shell_path: Option<PathBuf>) -> DetectedShell {
    get_shell(ShellType::PowerShell, /*path*/ None).unwrap_or_else(ultimate_fallback_shell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_detect_shell_type() {
        assert_eq!(
            detect_shell_type(PathBuf::from("zsh")),
            Some(ShellType::Zsh)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("bash")),
            Some(ShellType::Bash)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("powershell")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(detect_shell_type(PathBuf::from("fish")), None);
        assert_eq!(detect_shell_type(PathBuf::from("other")), None);
        assert_eq!(
            detect_shell_type(PathBuf::from("/bin/zsh")),
            Some(ShellType::Zsh)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("/bin/bash")),
            Some(ShellType::Bash)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("/usr/bin/bash")),
            Some(ShellType::Bash)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("powershell.exe")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("PowerShell.EXE")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from(
                "C:\\windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
            )),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("pwsh.exe")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("/usr/local/bin/pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("/bin/sh")),
            Some(ShellType::Sh)
        );
        assert_eq!(detect_shell_type(PathBuf::from("sh")), Some(ShellType::Sh));
        assert_eq!(
            detect_shell_type(PathBuf::from("cmd")),
            Some(ShellType::Cmd)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("cmd.exe")),
            Some(ShellType::Cmd)
        );
        assert_eq!(
            detect_shell_type(PathBuf::from("CMD.EXE")),
            Some(ShellType::Cmd)
        );
    }

    #[test]
    fn model_provided_shell_rejects_non_windows_shells() {
        assert_eq!(
            get_shell_by_model_provided_path(&PathBuf::from("bash")),
            None
        );
        assert_eq!(
            get_shell_by_model_provided_path(&PathBuf::from("zsh")),
            None
        );
        assert_eq!(get_shell_by_model_provided_path(&PathBuf::from("sh")), None);
    }

    #[test]
    fn powershell_resolver_prefers_pwsh_then_compatibility_host() {
        let pwsh = PathBuf::from("C:/Program Files/PowerShell/7/pwsh.exe");
        let default = PathBuf::from("D:/custom/powershell.exe");
        let compatibility = PathBuf::from("C:/Windows/System32/powershell.exe");

        assert_eq!(
            select_powershell_host(
                None,
                Some(pwsh.clone()),
                Some(default.clone()),
                Some(compatibility.clone())
            ),
            Some(pwsh)
        );
        assert_eq!(
            select_powershell_host(
                None,
                None,
                Some(default.clone()),
                Some(compatibility.clone())
            ),
            Some(default)
        );
        assert_eq!(
            select_powershell_host(None, None, None, Some(compatibility.clone())),
            Some(compatibility)
        );
    }

    #[test]
    fn explicit_powershell_host_overrides_preference_order() {
        let provided = PathBuf::from("D:/pinned/powershell.exe");
        let pwsh = PathBuf::from("C:/Program Files/PowerShell/7/pwsh.exe");

        assert_eq!(
            select_powershell_host(Some(provided.clone()), Some(pwsh), None, None),
            Some(provided)
        );
        assert_eq!(
            powershell_host_kind("C:/Program Files/PowerShell/7/pwsh.exe"),
            Some(PowerShellHostKind::Pwsh)
        );
        assert_eq!(
            powershell_host_kind("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"),
            Some(PowerShellHostKind::WindowsPowerShell)
        );
    }
}
