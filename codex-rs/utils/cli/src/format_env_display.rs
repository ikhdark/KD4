use std::collections::HashMap;

pub fn format_env_display<S: AsRef<str>>(
    env: Option<&HashMap<String, String>>,
    env_vars: &[S],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(map) = env {
        let mut pairs: Vec<_> = map.iter().collect();
        pairs.sort_by_key(|(key, _)| *key);
        parts.extend(pairs.into_iter().map(|(key, _)| format!("{key}=*****")));
    }

    if !env_vars.is_empty() {
        parts.extend(env_vars.iter().map(|var| format!("{}=*****", var.as_ref())));
    }

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_values_and_preserves_sorted_pairs_then_variable_order() {
        let empty_map = HashMap::new();
        let mut env = HashMap::new();
        env.insert("B".to_string(), "two".to_string());
        env.insert("A".to_string(), "one".to_string());
        let vars = ["TOKEN", "PATH"];
        for (map, variables, expected) in [
            (None, &[][..], "-"),
            (Some(&empty_map), &[][..], "-"),
            (Some(&env), &[][..], "A=*****, B=*****"),
            (None, &vars[..], "TOKEN=*****, PATH=*****"),
            (Some(&env), &vars[..], "A=*****, B=*****, TOKEN=*****, PATH=*****"),
        ] {
            assert_eq!(format_env_display(map, variables), expected);
        }
    }
}
