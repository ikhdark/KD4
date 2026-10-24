use std::fs;
use std::path::Path;

use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructions as Instructions;
use codex_extension_api::UserInstructionsProvider;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::CodexHomeUserInstructionsProvider;
use super::DEFAULT_AGENTS_MD_FILENAME;
use super::LOCAL_AGENTS_MD_FILENAME;

fn provider(home: &TempDir) -> CodexHomeUserInstructionsProvider {
    CodexHomeUserInstructionsProvider::new(
        AbsolutePathBuf::try_from(home.path().to_path_buf()).expect("absolute temp dir"),
    )
}

fn expected(
    home: &TempDir,
    filename: &str,
    text: &str,
    warnings: Vec<String>,
) -> LoadedUserInstructions {
    LoadedUserInstructions {
        instructions: Some(Instructions {
            text: text.to_string(),
            source: AbsolutePathBuf::try_from(home.path().join(filename))
                .expect("absolute source path"),
        }),
        warnings,
    }
}

#[cfg(unix)]
fn create_symlink_loop(path: &Path) {
    std::os::unix::fs::symlink(
        path.file_name().expect("override path should have a name"),
        path,
    )
    .expect("create symlink loop");
}

#[cfg(windows)]
fn create_symlink_loop(path: &Path) {
    std::os::windows::fs::symlink_file(
        path.file_name().expect("override path should have a name"),
        path,
    )
    .expect("create symlink loop");
}

#[tokio::test]
async fn missing_files_return_no_instructions() {
    let home = TempDir::new().expect("temp dir");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        LoadedUserInstructions::default()
    );
}

#[tokio::test]
async fn override_takes_precedence_over_default() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");
    fs::write(home.path().join(LOCAL_AGENTS_MD_FILENAME), "override").expect("write override");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(&home, LOCAL_AGENTS_MD_FILENAME, "override", Vec::new())
    );
}

#[tokio::test]
async fn empty_override_falls_back_to_trimmed_default() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(LOCAL_AGENTS_MD_FILENAME), " \n\t").expect("write override");
    fs::write(
        home.path().join(DEFAULT_AGENTS_MD_FILENAME),
        "\n  default instructions  \n",
    )
    .expect("write default");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "default instructions",
            Vec::new()
        )
    );
}

#[tokio::test]
async fn directory_override_falls_back_to_default() {
    let home = TempDir::new().expect("temp dir");
    fs::create_dir(home.path().join(LOCAL_AGENTS_MD_FILENAME)).expect("create override directory");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );
}

#[tokio::test]
async fn recoverable_override_read_error_warns_and_falls_back_to_default() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let override_path = home.path().join(LOCAL_AGENTS_MD_FILENAME);
    create_symlink_loop(&override_path);
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");
    let read_error = fs::read(&override_path).expect_err("symlink loop should not be readable");
    let warning = format!(
        "Failed to read global AGENTS.md instructions from `{}`: {read_error}",
        override_path.display()
    );

    assert_eq!(
        provider.load_user_instructions().await,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "default",
            vec![warning.clone()]
        )
    );
    assert_eq!(
        provider.clone().load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );

    fs::remove_file(&override_path).expect("remove broken symlink");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );
    create_symlink_loop(&override_path);
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", vec![warning])
    );
}

#[tokio::test]
async fn failed_refresh_retains_last_success_until_recovery_or_removal() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let path = home.path().join(DEFAULT_AGENTS_MD_FILENAME);
    fs::write(&path, "initial").expect("write initial instructions");
    let initial = provider.load_user_instructions().await;
    assert_eq!(
        initial,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "initial", Vec::new()),
    );

    fs::remove_file(&path).expect("remove initial file");
    create_symlink_loop(&path);
    let failed = provider.load_user_instructions().await;
    assert_eq!(failed.instructions, initial.instructions);
    assert_eq!(failed.warnings.len(), 1);
    assert_eq!(provider.load_user_instructions().await, initial);

    fs::remove_file(&path).expect("remove broken symlink");
    fs::write(&path, "recovered").expect("restore instructions");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "recovered", Vec::new()),
    );

    fs::write(&path, " \n").expect("clear instructions");
    assert_eq!(
        provider.load_user_instructions().await,
        LoadedUserInstructions::default(),
    );
    fs::remove_file(&path).expect("remove cleared instructions");
    create_symlink_loop(&path);
    let failed_after_clear = provider.load_user_instructions().await;
    assert_eq!(failed_after_clear.instructions, None);
    assert_eq!(failed_after_clear.warnings.len(), 1);
    fs::remove_file(&path).expect("remove broken symlink");
    assert_eq!(
        provider.load_user_instructions().await,
        LoadedUserInstructions::default(),
    );
}

#[tokio::test]
async fn failed_override_refresh_retains_last_good_over_readable_default() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let override_path = home.path().join(LOCAL_AGENTS_MD_FILENAME);
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "fallback").expect("write default");
    fs::write(&override_path, "binding override").expect("write override");
    let initial = provider.load_user_instructions().await;
    assert_eq!(
        initial,
        expected(&home, LOCAL_AGENTS_MD_FILENAME, "binding override", Vec::new()),
    );

    fs::remove_file(&override_path).expect("remove override");
    create_symlink_loop(&override_path);
    let failed = provider.load_user_instructions().await;
    assert_eq!(failed.instructions, initial.instructions);
    assert_eq!(failed.warnings.len(), 1);
    assert_eq!(provider.clone().load_user_instructions().await, initial);

    fs::remove_file(&override_path).expect("remove broken symlink");
    fs::write(&override_path, "recovered override").expect("restore override");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, LOCAL_AGENTS_MD_FILENAME, "recovered override", Vec::new()),
    );
    fs::remove_file(&override_path).expect("confirm override removal");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "fallback", Vec::new()),
    );
}

#[tokio::test]
async fn removed_override_is_not_restored_when_default_read_fails() {
    assert_unavailable_override_is_not_restored(None).await;
}

#[tokio::test]
async fn empty_override_is_not_restored_when_default_read_fails() {
    assert_unavailable_override_is_not_restored(Some(" \n\t")).await;
}

async fn assert_unavailable_override_is_not_restored(replacement: Option<&str>) {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let override_path = home.path().join(LOCAL_AGENTS_MD_FILENAME);
    fs::write(&override_path, "old override").expect("write override");
    assert!(
        provider
            .load_user_instructions()
            .await
            .instructions
            .is_some()
    );
    if let Some(contents) = replacement {
        fs::write(&override_path, contents).expect("empty override");
    } else {
        fs::remove_file(&override_path).expect("remove override");
    }
    create_symlink_loop(&home.path().join(DEFAULT_AGENTS_MD_FILENAME));
    let refreshed = provider.load_user_instructions().await;
    assert_eq!(refreshed.instructions, None);
    assert_eq!(refreshed.warnings.len(), 1);
    assert_eq!(
        provider.clone().load_user_instructions().await,
        LoadedUserInstructions::default()
    );
}

#[tokio::test]
async fn cancelled_refresh_releases_serialized_provider() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "current").expect("write default");
    let held_state = provider.state.lock().await;
    let clone = provider.clone();
    let blocked = tokio::spawn(async move { clone.load_user_instructions().await });
    tokio::task::yield_now().await;
    assert!(!blocked.is_finished());
    blocked.abort();
    assert!(blocked.await.expect_err("cancelled refresh").is_cancelled());
    drop(held_state);
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "current", Vec::new())
    );
}

#[tokio::test]
async fn invalid_utf8_is_lossy() {
    let home = TempDir::new().expect("temp dir");
    let path = home.path().join(DEFAULT_AGENTS_MD_FILENAME);
    let mut invalid_utf8 = b"global".to_vec();
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(b" doc");
    fs::write(&path, &invalid_utf8).expect("write invalid utf-8");

    let outcome = provider(&home).load_user_instructions().await;
    assert_eq!(
        outcome,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "global\u{fffd} doc",
            Vec::new()
        )
    );
}
