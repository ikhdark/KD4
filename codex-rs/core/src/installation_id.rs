use std::fs::OpenOptions;
use std::io::Read;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs;
use uuid::Uuid;

pub(crate) const INSTALLATION_ID_FILENAME: &str = "installation_id";
const MAX_INSTALLATION_ID_BYTES: u64 = 128;

pub async fn resolve_installation_id(codex_home: &AbsolutePathBuf) -> Result<String> {
    let path = codex_home.join(INSTALLATION_ID_FILENAME);
    fs::create_dir_all(codex_home).await?;
    tokio::task::spawn_blocking(move || {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);

        let mut file = options.open(&path)?;
        file.lock()?;

        let mut contents = Vec::new();
        (&mut file)
            .take(MAX_INSTALLATION_ID_BYTES + 1)
            .read_to_end(&mut contents)?;
        if contents.len() <= MAX_INSTALLATION_ID_BYTES as usize {
            let contents = String::from_utf8(contents)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let trimmed = contents.trim();
            if !trimmed.is_empty()
                && let Ok(existing) = Uuid::parse_str(trimmed)
            {
                return Ok(existing.to_string());
            }
        }

        let installation_id = Uuid::new_v4().to_string();
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(installation_id.as_bytes())?;
        file.flush()?;
        file.sync_all()?;

        Ok(installation_id)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::INSTALLATION_ID_FILENAME;
    use super::resolve_installation_id;
    use core_test_support::PathExt;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[tokio::test]
    async fn resolve_installation_id_generates_and_persists_uuid() {
        let codex_home = TempDir::new().expect("create temp dir");
        let codex_home_abs = codex_home.path().abs();
        let persisted_path = codex_home.path().join(INSTALLATION_ID_FILENAME);

        let installation_id = resolve_installation_id(&codex_home_abs)
            .await
            .expect("resolve installation id");

        assert_eq!(
            std::fs::read_to_string(&persisted_path).expect("read persisted installation id"),
            installation_id
        );
        assert!(Uuid::parse_str(&installation_id).is_ok());
    }

    #[tokio::test]
    async fn resolve_installation_id_reuses_existing_uuid() {
        let codex_home = TempDir::new().expect("create temp dir");
        let codex_home_abs = codex_home.path().abs();
        let existing = Uuid::new_v4().to_string().to_uppercase();
        std::fs::write(
            codex_home.path().join(INSTALLATION_ID_FILENAME),
            existing.clone(),
        )
        .expect("write installation id");

        let resolved = resolve_installation_id(&codex_home_abs)
            .await
            .expect("resolve installation id");

        assert_eq!(
            resolved,
            Uuid::parse_str(existing.as_str())
                .expect("parse existing installation id")
                .to_string()
        );
    }

    #[tokio::test]
    async fn resolve_installation_id_replaces_oversized_record() {
        let codex_home = TempDir::new().expect("create temp dir");
        let existing = Uuid::new_v4().to_string();
        let path = codex_home.path().join(INSTALLATION_ID_FILENAME);
        std::fs::write(&path, format!("{existing}{}", " ".repeat(512 * 1024)))
            .expect("write oversized record");
        let resolved = resolve_installation_id(&codex_home.path().abs())
            .await
            .expect("replace oversized record");
        assert_ne!(resolved, existing);
        assert!(Uuid::parse_str(&resolved).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), resolved);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 36);
    }

    #[tokio::test]
    async fn resolve_installation_id_rewrites_invalid_file_contents() {
        let codex_home = TempDir::new().expect("create temp dir");
        let codex_home_abs = codex_home.path().abs();
        std::fs::write(
            codex_home.path().join(INSTALLATION_ID_FILENAME),
            "not-a-uuid",
        )
        .expect("write invalid installation id");

        let resolved = resolve_installation_id(&codex_home_abs)
            .await
            .expect("resolve installation id");

        assert!(Uuid::parse_str(&resolved).is_ok());
        assert_eq!(
            std::fs::read_to_string(codex_home.path().join(INSTALLATION_ID_FILENAME))
                .expect("read rewritten installation id"),
            resolved
        );
    }
}
