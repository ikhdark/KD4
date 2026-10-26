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
    "WINDIR",
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

