use std::io;
use std::io::Write as _;
use std::path::Path;

pub(crate) async fn write_generated_file(path: &Path, contents: &str) -> io::Result<()> {
    match tokio::fs::read(path).await {
        Ok(existing) if existing == contents.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    write_generated_file_atomic(path, contents).await
}

async fn write_generated_file_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let path = path.to_path_buf();
    let contents = contents.to_string();
    tokio::task::spawn_blocking(move || write_generated_file_atomic_sync(&path, contents.as_bytes()))
        .await
        .map_err(|err| io::Error::other(format!("generated file write task failed: {err}")))?
}

fn write_generated_file_atomic_sync(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("generated file path has no parent: {}", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;
    temp_file.write_all(contents)?;
    temp_file.flush()?;
    temp_file.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_generated_file_writes_atomically_and_removes_temp_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("nested").join("generated.md");

        write_generated_file(&path, "hello").await.expect("write file");

        assert_eq!(
            tokio::fs::read_to_string(&path).await.expect("read file"),
            "hello"
        );
        let mut dir = tokio::fs::read_dir(path.parent().expect("parent"))
            .await
            .expect("read parent");
        let mut names = Vec::new();
        while let Some(entry) = dir.next_entry().await.expect("read entry") {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
        assert_eq!(names, vec!["generated.md".to_string()]);
    }

    #[tokio::test]
    async fn write_generated_file_replaces_existing_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("generated.md");

        write_generated_file(&path, "old").await.expect("write old");
        write_generated_file(&path, "new").await.expect("write new");

        assert_eq!(
            tokio::fs::read_to_string(&path).await.expect("read file"),
            "new"
        );
        let mut dir = tokio::fs::read_dir(path.parent().expect("parent"))
            .await
            .expect("read parent");
        let mut names = Vec::new();
        while let Some(entry) = dir.next_entry().await.expect("read entry") {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
        assert_eq!(names, vec!["generated.md".to_string()]);
    }
}
