use crate::config_types::EnvironmentVariablePattern;
use crate::config_types::ShellEnvironmentPolicy;
use crate::config_types::ShellEnvironmentPolicyInherit;
use std::collections::HashMap;
use std::ffi::OsString;

pub const CODEX_THREAD_ID_ENV_VAR: &str = "CODEX_THREAD_ID";

/// Construct a shell environment from the supplied process environment and
/// shell-environment policy.
pub fn create_env(
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
) -> HashMap<String, String> {
    create_env_from_os_vars(std::env::vars_os(), policy, thread_id)
}

fn create_env_from_os_vars<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    create_env_from_vars(
        vars.into_iter()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
        policy,
        thread_id,
    )
}

pub fn create_env_from_vars<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    populate_env_for_platform(vars, policy, thread_id, cfg!(windows))
}

pub fn populate_env<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    populate_env_impl(
        vars,
        policy,
        thread_id,
        cfg!(windows),
        /*inject_pathext*/ false,
    )
}

fn populate_env_for_platform<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
    is_windows: bool,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    populate_env_impl(vars, policy, thread_id, is_windows, is_windows)
}

fn populate_env_impl<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
    is_windows: bool,
    inject_pathext: bool,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    // Step 1 - determine the starting set of variables based on the
    // `inherit` strategy.
    let mut env_map: HashMap<String, String> = match policy.inherit {
        ShellEnvironmentPolicyInherit::All => vars.into_iter().collect(),
        ShellEnvironmentPolicyInherit::None => HashMap::new(),
        ShellEnvironmentPolicyInherit::Core => {
            let core_env_vars = if is_windows {
                WINDOWS_CORE_ENV_VARS
            } else {
                UNIX_CORE_ENV_VARS
            };

            vars.into_iter()
                .filter(|(k, _)| {
                    core_env_vars
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(k))
                })
                .collect()
        }
    };

    // Windows command lookup needs PATHEXT, but the policy's explicit exclude
    // and include-only filters remain authoritative.
    if inject_pathext && !env_map.keys().any(|k| k.eq_ignore_ascii_case("PATHEXT")) {
        env_map.insert("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string());
    }

    let matches_any = |name: &str, patterns: &[EnvironmentVariablePattern]| -> bool {
        patterns.iter().any(|pattern| pattern.matches(name))
    };

    // Step 2 - Apply the default exclude if not disabled.
    if !policy.ignore_default_excludes {
        let default_excludes = vec![
            EnvironmentVariablePattern::new_case_insensitive("*KEY*"),
            EnvironmentVariablePattern::new_case_insensitive("*SECRET*"),
            EnvironmentVariablePattern::new_case_insensitive("*TOKEN*"),
        ];
        env_map.retain(|k, _| !matches_any(k, &default_excludes));
    }

    // Step 3 - Apply custom excludes.
    if !policy.exclude.is_empty() {
        env_map.retain(|k, _| !matches_any(k, &policy.exclude));
    }

    // Step 4 - Apply user-provided overrides.
    for (key, val) in &policy.r#set {
        env_map.insert(key.clone(), val.clone());
    }

    // Step 5 - If include_only is non-empty, keep only the matching vars.
    if !policy.include_only.is_empty() {
        env_map.retain(|k, _| matches_any(k, &policy.include_only));
    }

    // Step 6 - Populate the thread ID environment variable when provided.
    if let Some(thread_id) = thread_id {
        env_map.insert(CODEX_THREAD_ID_ENV_VAR.to_string(), thread_id.to_string());
    }

    env_map
}

pub const UNIX_CORE_ENV_VARS: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "USER"];

pub const WINDOWS_CORE_ENV_VARS: &[&str] = &[
    // Core path resolution
    "PATH",
    "PATHEXT",
    // Shell and system roots
    "SHELL",
    "COMSPEC",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    // User context and profiles
    "USERNAME",
    "USERDOMAIN",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    // Program locations
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    // App data and caches
    "LOCALAPPDATA",
    "APPDATA",
    // Temp locations
    "TEMP",
    "TMP",
    "TMPDIR",
    // Common shells/pwsh hints
    "POWERSHELL",
    "PWSH",
];

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn make_vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn core_inherit_preserves_windows_startup_vars_case_insensitively() {
        let vars = make_vars(&[
            ("Shell", "C:\\Program Files\\Git\\bin\\bash.exe"),
            ("SystemRoot", "C:\\Windows"),
            ("AppData", "C:\\Users\\codex\\AppData\\Roaming"),
            ("TmpDir", "C:\\Temp\\custom"),
            ("OPENAI_API_KEY", "secret"),
        ]);

        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::Core,
            ignore_default_excludes: true,
            ..Default::default()
        };

        // Check a few sample vars instead of the full Windows core list.
        let result = populate_env_for_platform(
            vars, &policy, /*thread_id*/ None, /*is_windows*/ true,
        );
        let expected = HashMap::from([
            (
                "Shell".to_string(),
                "C:\\Program Files\\Git\\bin\\bash.exe".to_string(),
            ),
            ("SystemRoot".to_string(), "C:\\Windows".to_string()),
            ("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
            (
                "AppData".to_string(),
                "C:\\Users\\codex\\AppData\\Roaming".to_string(),
            ),
            ("TmpDir".to_string(), "C:\\Temp\\custom".to_string()),
        ]);

        assert_eq!(result, expected);
    }

    #[test]
    fn create_env_inserts_pathext_on_windows_when_missing() {
        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: true,
            ..Default::default()
        };

        let result = populate_env_for_platform(
            Vec::new(),
            &policy,
            /*thread_id*/ None,
            /*is_windows*/ true,
        );
        let expected = HashMap::from([("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string())]);

        assert_eq!(result, expected);
    }

    #[test]
    fn core_inherit_preserves_unix_identity_vars() {
        let vars = make_vars(&[
            ("HOME", "/home/codex"),
            ("LOGNAME", "codex"),
            ("PATH", "/usr/bin"),
            ("SHELL", "/bin/sh"),
            ("USER", "codex"),
            ("USERPROFILE", "windows-only"),
        ]);
        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::Core,
            ignore_default_excludes: true,
            ..Default::default()
        };

        let result = populate_env_for_platform(vars, &policy, None, /*is_windows*/ false);

        assert_eq!(
            result,
            HashMap::from([
                ("HOME".to_string(), "/home/codex".to_string()),
                ("LOGNAME".to_string(), "codex".to_string()),
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
                ("USER".to_string(), "codex".to_string()),
            ])
        );
    }

    #[test]
    fn pathext_is_not_injected_on_unix_or_past_policy_filters() {
        let none_policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: true,
            ..Default::default()
        };
        assert_eq!(
            populate_env_for_platform(Vec::new(), &none_policy, None, false),
            HashMap::new()
        );

        let filtered_policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: true,
            include_only: vec![EnvironmentVariablePattern::new_case_insensitive("HOME")],
            ..Default::default()
        };
        assert_eq!(
            populate_env_for_platform(Vec::new(), &filtered_policy, None, true),
            HashMap::new()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_utf8_process_entries_are_skipped_without_panicking() {
        #[cfg(unix)]
        fn non_utf8_os_string() -> OsString {
            use std::os::unix::ffi::OsStringExt;

            OsString::from_vec(vec![0xff])
        }

        #[cfg(windows)]
        fn non_utf8_os_string() -> OsString {
            use std::os::windows::ffi::OsStringExt;

            OsString::from_wide(&[0xd800])
        }

        let vars = [
            (non_utf8_os_string(), OsString::from("unrelated")),
            (OsString::from("PATH"), OsString::from("/bin")),
        ];
        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::Core,
            ignore_default_excludes: true,
            include_only: vec![EnvironmentVariablePattern::new_case_insensitive("PATH")],
            ..Default::default()
        };

        assert_eq!(
            create_env_from_os_vars(vars, &policy, None),
            HashMap::from([("PATH".to_string(), "/bin".to_string())])
        );
    }
}
