use super::runtime_paths;
use pretty_assertions::assert_eq;
use std::path::PathBuf;

#[test]
fn runtime_paths_include_only_available_runtime_roots() {
    let local_app_data = PathBuf::from(r"C:\Users\user\AppData\Local");
    let user_profile = PathBuf::from(r"C:\Users\user");
    let desktop = vec![
        PathBuf::from(r"C:\Users\user\AppData\Local\OpenAI\Codex\bin"),
        PathBuf::from(r"C:\Users\user\AppData\Local\OpenAI\Codex\runtimes"),
    ];
    let primary = PathBuf::from(r"C:\Users\user\.cache\codex-runtimes");
    let mut both = desktop.clone();
    both.push(primary.clone());
    for (local, profile, expected) in [
        (Some(local_app_data.clone()), Some(user_profile.clone()), both),
        (Some(local_app_data), None, desktop),
        (None, Some(user_profile), vec![primary]),
        (None, None, vec![]),
    ] {
        assert_eq!(runtime_paths(local, profile), expected);
    }
}
