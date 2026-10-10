use crate::config_types::EnvironmentVariablePattern;
use crate::config_types::ShellEnvironmentPolicy;
use crate::config_types::ShellEnvironmentPolicyInherit;
use std::collections::HashMap;
use std::ffi::OsString;

pub const CODEX_THREAD_ID_ENV_VAR: &str = "CODEX_THREAD_ID";

/// Apply an explicit overlay after an earlier environment layer.
///
/// Windows names are case-insensitive. Within a layer, the lexicographically
/// last spelling wins, independently of HashMap iteration order. Across layers,
/// the overlay always wins.
pub fn apply_env_overlay(env: &mut HashMap<String, String>, overlay: HashMap<String, String>) {
    // Normalize both layers, including inherited or exact environments which
    // may already contain aliases. Do not sort the combined layers: doing so
    // would let spelling override explicit overlay precedence.
    let inherited = std::mem::take(env);
    for layer in [inherited, overlay] {
        let mut entries = layer.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        for (key, value) in entries {
            env.retain(|existing, _| !existing.eq_ignore_ascii_case(&key));
            env.insert(key, value);
        }
    }
}

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
    populate_env_impl(vars, policy, thread_id, /*inject_pathext*/ true)
}

pub fn populate_env<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    populate_env_impl(vars, policy, thread_id, /*inject_pathext*/ false)
}

fn populate_env_impl<I>(
    vars: I,
    policy: &ShellEnvironmentPolicy,
    thread_id: Option<&str>,
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
        ShellEnvironmentPolicyInherit::Core => vars
            .into_iter()
            .filter(|(k, _)| {
                WINDOWS_CORE_ENV_VARS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(k))
            })
            .collect(),
    };

    apply_env_overlay(&mut env_map, HashMap::new());

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
    apply_env_overlay(&mut env_map, policy.r#set.clone());

    // Step 5 - If include_only is non-empty, keep only the matching vars.
    if !policy.include_only.is_empty() {
        env_map.retain(|k, _| matches_any(k, &policy.include_only));
    }

    // Step 6 - Populate the thread ID environment variable when provided.
    if let Some(thread_id) = thread_id {
        apply_env_overlay(
            &mut env_map,
            HashMap::from([(CODEX_THREAD_ID_ENV_VAR.to_string(), thread_id.to_string())]),
        );
    }

    env_map
}

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
    fn environment_overlay_precedence_is_independent_of_case_and_insertion_order() {
        for inherited in [
            [("PATH", "upper"), ("Path", "mixed")],
            [("Path", "mixed"), ("PATH", "upper")],
        ] {
            for overlay in [
                [("PATH", "overlay-upper"), ("Path", "overlay-mixed")],
                [("Path", "overlay-mixed"), ("PATH", "overlay-upper")],
            ] {
                let mut env = make_vars(&inherited).into_iter().collect();
                apply_env_overlay(&mut env, HashMap::new());
                assert_eq!(env, HashMap::from([("Path".into(), "mixed".into())]));
                apply_env_overlay(&mut env, make_vars(&overlay).into_iter().collect());
                assert_eq!(env, HashMap::from([("Path".into(), "overlay-mixed".into())]));
                apply_env_overlay(
                    &mut env,
                    HashMap::from([("PATH".into(), "last-layer".into())]),
                );
                assert_eq!(env, HashMap::from([("PATH".into(), "last-layer".into())]));
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn public_environment_overrides_reach_windows_child_without_case_aliases() {
        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::All,
            r#set: HashMap::from([
                ("PATH".to_string(), "replacement-path".to_string()),
                ("Codex_Thread_Id".to_string(), "policy-thread".to_string()),
            ]),
            ..Default::default()
        };
        let env = create_env_from_vars(
            make_vars(&[
                ("Path", "inherited-path"),
                ("codex_thread_id", "old-thread"),
            ]),
            &policy,
            Some("current-thread"),
        );
        assert_eq!(
            env,
            HashMap::from([
                ("PATH".to_string(), "replacement-path".to_string()),
                ("CODEX_THREAD_ID".to_string(), "current-thread".to_string()),
                ("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string()),
            ])
        );

        let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        let output = std::process::Command::new(cmd)
            .args(["/d", "/c", "echo %PATH%;%CODEX_THREAD_ID%"])
            .env_clear()
            .envs(env)
            .output()
            .expect("run Windows child with the public environment");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("ASCII child output"),
            "replacement-path;current-thread\r\n"
        );
    }

    #[test]
    #[cfg(windows)]
    fn explicit_windows_aliases_have_deterministic_precedence() {
        for pairs in [
            [("PATH", "upper"), ("Path", "mixed")],
            [("Path", "mixed"), ("PATH", "upper")],
        ] {
            let policy = ShellEnvironmentPolicy {
                inherit: ShellEnvironmentPolicyInherit::None,
                r#set: make_vars(&pairs).into_iter().collect(),
                include_only: vec![EnvironmentVariablePattern::new_case_insensitive("PATH")],
                ..Default::default()
            };
            assert_eq!(
                create_env_from_vars(Vec::new(), &policy, None),
                HashMap::from([("Path".to_string(), "mixed".to_string())])
            );
        }
    }

    #[test]
    fn public_environment_uses_native_platform_semantics() {
        let vars = make_vars(&[
            ("HOME", "/home/user"),
            ("USER", "user"),
            ("USERPROFILE", "C:\\Users\\user"),
            ("PATH", "upper"),
            ("Path", "mixed"),
        ]);
        let policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::Core,
            ..Default::default()
        };
        let expected = HashMap::from([
            ("USERPROFILE".to_string(), "C:\\Users\\user".to_string()),
            ("Path".to_string(), "mixed".to_string()),
        ]);
        assert_eq!(populate_env(vars.clone(), &policy, None), expected);
        let mut expected_with_lookup_defaults = expected;
        expected_with_lookup_defaults.insert(
            "PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string(),
        );
        assert_eq!(
            create_env_from_vars(vars, &policy, None),
            expected_with_lookup_defaults,
        );
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
        let result = create_env_from_vars(vars, &policy, /*thread_id*/ None);
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

        let result = create_env_from_vars(Vec::new(), &policy, /*thread_id*/ None);
        let expected = HashMap::from([("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string())]);

        assert_eq!(result, expected);
    }

    #[test]
    fn pathext_is_not_injected_past_policy_filters() {
        let filtered_policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: true,
            include_only: vec![EnvironmentVariablePattern::new_case_insensitive("HOME")],
            ..Default::default()
        };
        assert_eq!(
            create_env_from_vars(Vec::new(), &filtered_policy, None),
            HashMap::new()
        );
    }

    #[test]
    fn non_utf8_process_entries_are_skipped_without_panicking() {
        #[cfg(windows)]
        fn non_utf8_os_string() -> OsString {
            use std::os::windows::ffi::OsStringExt;

            OsString::from_wide(&[0xd800])
        }

        #[cfg(unix)]
        fn non_utf8_os_string() -> OsString {
            use std::os::unix::ffi::OsStringExt;

            OsString::from_vec(vec![0xff])
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
