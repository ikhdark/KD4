use anyhow::Result;
use std::collections::HashMap;
use std::env;

/// Windows variable names use ordinal, case-insensitive identity. Normalize
/// before policy/default insertion, rejecting ambiguous caller overrides.
pub(crate) fn normalize_environment(env_map: &mut HashMap<String, String>) -> Result<()> {
    use windows_sys::Win32::Globalization::CompareStringOrdinal;
    let mut entries = env_map.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut normalized = HashMap::new();
    let mut names: Vec<(Vec<u16>, &String)> = Vec::new();
    for (key, value) in entries {
        let drive_directory = key.len() == 3 && key.starts_with('=') && key.as_bytes()[1].is_ascii_alphabetic() && key.ends_with(':');
        anyhow::ensure!(!key.is_empty() && !key.contains('\0') && (!key.contains('=') || drive_directory) && !value.contains('\0'), "invalid Windows environment record");
        let wide: Vec<u16> = key.encode_utf16().collect();
        anyhow::ensure!(wide.len() <= 32767, "Windows environment name is too long");
        let duplicate = names.iter().find(|(name, _)| {
            // SAFETY: Both counted UTF-16 slices remain live for the call;
            // lengths were checked and no NUL-terminated access is requested.
            unsafe { CompareStringOrdinal(name.as_ptr(), name.len() as i32, wide.as_ptr(), wide.len() as i32, 1) == 2 }
        });
        if let Some((_, previous)) = duplicate {
            anyhow::ensure!(*previous == value, "conflicting case-equivalent Windows environment names");
            continue;
        }
        names.push((wide, value));
        normalized.insert(key.to_ascii_uppercase(), value.clone());
    }
    *env_map = normalized;
    Ok(())
}

pub fn normalize_null_device_env(env_map: &mut HashMap<String, String>) {
    let keys: Vec<String> = env_map.keys().cloned().collect();
    for k in keys {
        if let Some(v) = env_map.get(&k).cloned() {
            let t = v.trim().to_ascii_lowercase();
            if t == "/dev/null" || t == "\\\\\\\\dev\\\\\\\\null" {
                env_map.insert(k, "NUL".to_string());
            }
        }
    }
}

pub fn ensure_non_interactive_pager(env_map: &mut HashMap<String, String>) {
    env_map
        .entry("GIT_PAGER".into())
        .or_insert_with(|| "more.com".into());
    env_map
        .entry("PAGER".into())
        .or_insert_with(|| "more.com".into());
    env_map.entry("LESS".into()).or_insert_with(|| "".into());
}

// Keep PATH and PATHEXT stable for callers that rely on inheriting the parent process env.
pub fn inherit_path_env(env_map: &mut HashMap<String, String>) {
    if !env_map.contains_key("PATH")
        && let Ok(path) = env::var("PATH")
    {
        env_map.insert("PATH".into(), path);
    }
    if !env_map.contains_key("PATHEXT")
        && let Ok(pathext) = env::var("PATHEXT")
    {
        env_map.insert("PATHEXT".into(), pathext);
    }
}


