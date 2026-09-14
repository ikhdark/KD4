use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn backup_moves_only_requested_runtime_db_files_to_backup_folder() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let sqlite_home = temp.path().to_path_buf();
    tokio::fs::create_dir_all(sqlite_home.as_path()).await?;
    let runtime_paths = super::super::runtime_db_paths(sqlite_home.as_path());
    let mut expected_paths = Vec::new();
    for db_path in runtime_paths.iter().map(|db| db.path.as_path()) {
        for path in sqlite_paths(db_path) {
            tokio::fs::write(path.as_path(), path.display().to_string()).await?;
            expected_paths.push(path);
        }
    }
    let failed_db_path = super::super::logs_db_path(sqlite_home.as_path());
    let failed_paths = sqlite_paths(failed_db_path.as_path());

    let backups = backup_runtime_db_for_fresh_start(failed_db_path.as_path()).await?;

    assert_eq!(backups.len(), failed_paths.len());
    for path in &failed_paths {
        assert!(!tokio::fs::try_exists(path.as_path()).await?);
    }
    for path in expected_paths
        .iter()
        .filter(|path| !failed_paths.contains(path))
    {
        assert_eq!(
            tokio::fs::read(path).await?,
            path.display().to_string().as_bytes()
        );
    }
    for backup in backups {
        assert!(
            backup
                .backup_path
                .starts_with(sqlite_home.join(BACKUP_DIR_NAME))
        );
        assert_eq!(
            tokio::fs::read(&backup.backup_path).await?,
            backup.original_path.display().to_string().as_bytes()
        );
    }
    Ok(())
}

#[tokio::test]
async fn backup_replaces_blocking_sqlite_home_file() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let temp_dir = temp.path().to_path_buf();
    tokio::fs::create_dir_all(temp_dir.as_path()).await?;
    let sqlite_home = temp_dir.join("sqlite-home");
    tokio::fs::write(sqlite_home.as_path(), b"not-a-directory").await?;

    let backups = backup_runtime_db_for_fresh_start(
        super::super::state_db_path(sqlite_home.as_path()).as_path(),
    )
    .await?;

    assert_eq!(backups.len(), 1);
    assert!(tokio::fs::metadata(sqlite_home.as_path()).await?.is_dir());
    assert!(
        backups[0]
            .backup_path
            .starts_with(temp_dir.join(format!("sqlite-home.{BACKUP_DIR_NAME}")))
    );
    assert_eq!(
        tokio::fs::read(&backups[0].backup_path).await?,
        b"not-a-directory"
    );
    Ok(())
}

#[test]
fn sqlite_error_detail_classifies_corruption_and_lock_errors() {
    assert!(sqlite_error_detail_is_corruption("file is not a database"));
    assert!(sqlite_error_detail_is_corruption(
        "error returned from database: (code: 11) database disk image is malformed"
    ));
    assert!(!sqlite_error_detail_is_corruption("database is locked"));
    assert!(sqlite_error_detail_is_lock("database is locked"));
    assert!(sqlite_error_detail_is_lock("database is busy"));
}

#[tokio::test]
async fn runtime_db_path_for_corruption_error_returns_failed_database_path() -> std::io::Result<()>
{
    let temp = tempfile::tempdir()?;
    let sqlite_home = temp.path().to_path_buf();
    tokio::fs::create_dir_all(sqlite_home.as_path()).await?;
    let path = super::super::state_db_path(sqlite_home.as_path());
    tokio::fs::write(path.as_path(), b"not sqlite").await?;

    let err = match super::super::StateRuntime::init(sqlite_home, "openai".to_string()).await {
        Ok(_) => panic!("malformed sqlite should fail to initialize"),
        Err(err) => err,
    };

    assert_eq!(runtime_db_path_for_corruption_error(&err), Some(path));
    Ok(())
}

#[test]
fn runtime_db_path_for_corruption_error_ignores_corrupt_word_in_path() {
    let path = PathBuf::from("/tmp/sqlite_corrupt/state_5.sqlite");
    let err = anyhow::Error::new(RuntimeDbInitError::new(
        "state DB",
        "open",
        path.as_path(),
        anyhow::anyhow!("permission denied"),
    ));

    assert_eq!(runtime_db_path_for_corruption_error(&err), None);
}

#[tokio::test]
async fn partial_backup_failure_reports_and_preserves_completed_moves() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let first = temp.path().join("state.sqlite");
    let second = temp.path().join("other/state.sqlite");
    tokio::fs::write(&first, b"original database").await?;
    tokio::fs::create_dir_all(&second).await?;
    tokio::fs::write(second.join("contents"), b"untouched").await?;
    let error = backup_sqlite_paths(temp.path(), [first.clone(), second.clone()])
        .await
        .expect_err("directory cannot replace backed-up file");
    let detail = error
        .get_ref()
        .unwrap()
        .downcast_ref::<PartialBackupError>()
        .expect("partial backup context");
    assert_eq!(detail.backups.len(), 1);
    assert_eq!(detail.backups[0].original_path, first);
    assert_eq!(
        tokio::fs::read(&detail.backups[0].backup_path).await?,
        b"original database"
    );
    assert_eq!(
        tokio::fs::read(second.join("contents")).await?,
        b"untouched"
    );
    assert!(
        error
            .to_string()
            .contains(&detail.backups[0].backup_path.display().to_string())
    );
    Ok(())
}
