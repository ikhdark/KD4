use codex_utils_absolute_path::AbsolutePathBuf;
use include_dir::Dir;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;

use thiserror::Error;

const SYSTEM_SKILLS_DIR: Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/assets/samples");

const SYSTEM_SKILLS_DIR_NAME: &str = ".system";
const SKILLS_DIR_NAME: &str = "skills";
const SYSTEM_SKILLS_MARKER_FILENAME: &str = ".codex-system-skills.marker";
const SYSTEM_SKILLS_MARKER_SALT: &str = "v1";

/// Returns the on-disk cache location for embedded system skills from an absolute CODEX_HOME.
pub fn system_cache_root_dir(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    codex_home
        .join(SKILLS_DIR_NAME)
        .join(SYSTEM_SKILLS_DIR_NAME)
}

/// Installs embedded system skills into `CODEX_HOME/skills/.system`.
///
/// Stages a complete replacement before publishing it. Shared-home writers are
/// serialized, and a failed publication restores the previous installation.
///
/// To avoid doing unnecessary work on every startup, a marker file is written
/// with a fingerprint of the embedded directory. When the marker matches, the
/// install is skipped.
pub fn install_system_skills(codex_home: &AbsolutePathBuf) -> Result<(), SystemSkillsError> {
    install_system_skills_with(codex_home, |dest| {
        write_embedded_dir(&SYSTEM_SKILLS_DIR, dest)
    })
}

fn install_system_skills_with(
    codex_home: &AbsolutePathBuf,
    write_skills: impl FnOnce(&AbsolutePathBuf) -> Result<(), SystemSkillsError>,
) -> Result<(), SystemSkillsError> {
    let _lock = lock_system_skills(codex_home)?;
    let skills_root_dir = codex_home.join(SKILLS_DIR_NAME);
    let backup = skills_root_dir.join(".system-backup");
    let dest_system = system_cache_root_dir(codex_home);

    // Recover a process exit between moving the previous tree and publishing the new one.
    if backup.as_path().exists() {
        if dest_system.as_path().exists() {
            fs::remove_dir_all(backup.as_path()).map_err(|source| {
                SystemSkillsError::io("remove previous system skills backup", source)
            })?;
        } else {
            fs::rename(backup.as_path(), dest_system.as_path())
                .map_err(|source| SystemSkillsError::io("restore system skills backup", source))?;
        }
    }

    let marker_path = dest_system.join(SYSTEM_SKILLS_MARKER_FILENAME);
    let expected_fingerprint = embedded_system_skills_fingerprint();
    if dest_system.as_path().is_dir()
        && read_marker(&marker_path).is_ok_and(|marker| marker == expected_fingerprint)
    {
        return Ok(());
    }

    let staging = tempfile::Builder::new()
        .prefix(".system-staging-")
        .tempdir_in(skills_root_dir.as_path())
        .map_err(|source| SystemSkillsError::io("create system skills staging dir", source))?;
    let staged_system = AbsolutePathBuf::from_absolute_path(staging.path())
        .map_err(|source| SystemSkillsError::io("resolve system skills staging dir", source))?;
    write_skills(&staged_system)?;
    fs::write(
        staged_system.join(SYSTEM_SKILLS_MARKER_FILENAME).as_path(),
        format!("{expected_fingerprint}\n"),
    )
    .map_err(|source| SystemSkillsError::io("write system skills marker", source))?;

    let had_previous = dest_system.as_path().exists();
    if had_previous {
        fs::rename(dest_system.as_path(), backup.as_path()).map_err(|source| {
            SystemSkillsError::io("back up existing system skills dir", source)
        })?;
    }
    if let Err(source) = fs::rename(staging.path(), dest_system.as_path()) {
        if had_previous {
            fs::rename(backup.as_path(), dest_system.as_path())
                .map_err(|source| SystemSkillsError::io("restore system skills backup", source))?;
        }
        return Err(SystemSkillsError::io("publish system skills dir", source));
    }
    if had_previous {
        fs::remove_dir_all(backup.as_path())
            .map_err(|source| SystemSkillsError::io("remove system skills backup", source))?;
    }
    Ok(())
}

fn lock_system_skills(codex_home: &AbsolutePathBuf) -> Result<fs::File, SystemSkillsError> {
    let root = codex_home.join(SKILLS_DIR_NAME);
    fs::create_dir_all(root.as_path())
        .map_err(|source| SystemSkillsError::io("create skills root dir", source))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".system.lock").as_path())
        .map_err(|source| SystemSkillsError::io("open system skills lock", source))?;
    lock.lock()
        .map_err(|source| SystemSkillsError::io("lock system skills installation", source))?;
    Ok(lock)
}

/// Remove cached system skills using the same shared-home lock as installation.
pub fn uninstall_system_skills(codex_home: &AbsolutePathBuf) -> Result<(), SystemSkillsError> {
    let _lock = lock_system_skills(codex_home)?;
    for path in [
        system_cache_root_dir(codex_home),
        codex_home.join(SKILLS_DIR_NAME).join(".system-backup"),
    ] {
        match fs::remove_dir_all(path.as_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(SystemSkillsError::io("remove system skills dir", source)),
        }
    }
    Ok(())
}

fn read_marker(path: &AbsolutePathBuf) -> Result<String, SystemSkillsError> {
    Ok(fs::read_to_string(path.as_path())
        .map_err(|source| SystemSkillsError::io("read system skills marker", source))?
        .trim()
        .to_string())
}

fn embedded_system_skills_fingerprint() -> String {
    let mut items = Vec::new();
    collect_fingerprint_items(&SYSTEM_SKILLS_DIR, &mut items);
    items.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    let mut hasher = DefaultHasher::new();
    SYSTEM_SKILLS_MARKER_SALT.hash(&mut hasher);
    for (path, contents_hash) in items {
        path.hash(&mut hasher);
        contents_hash.hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

fn collect_fingerprint_items(dir: &Dir<'_>, items: &mut Vec<(String, Option<u64>)>) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(subdir) => {
                items.push((subdir.path().to_string_lossy().to_string(), None));
                collect_fingerprint_items(subdir, items);
            }
            include_dir::DirEntry::File(file) => {
                let mut file_hasher = DefaultHasher::new();
                file.contents().hash(&mut file_hasher);
                items.push((
                    file.path().to_string_lossy().to_string(),
                    Some(file_hasher.finish()),
                ));
            }
        }
    }
}

/// Writes the embedded `include_dir::Dir` to disk under `dest`.
///
/// Preserves the embedded directory structure.
fn write_embedded_dir(dir: &Dir<'_>, dest: &AbsolutePathBuf) -> Result<(), SystemSkillsError> {
    fs::create_dir_all(dest.as_path())
        .map_err(|source| SystemSkillsError::io("create system skills dir", source))?;

    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(subdir) => {
                let subdir_dest = dest.join(subdir.path());
                fs::create_dir_all(subdir_dest.as_path()).map_err(|source| {
                    SystemSkillsError::io("create system skills subdir", source)
                })?;
                write_embedded_dir(subdir, dest)?;
            }
            include_dir::DirEntry::File(file) => {
                let path = dest.join(file.path());
                if let Some(parent) = path.as_path().parent() {
                    fs::create_dir_all(parent).map_err(|source| {
                        SystemSkillsError::io("create system skills file parent", source)
                    })?;
                }
                fs::write(path.as_path(), file.contents())
                    .map_err(|source| SystemSkillsError::io("write system skill file", source))?;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum SystemSkillsError {
    #[error("io error while {action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl SystemSkillsError {
    fn io(action: &'static str, source: std::io::Error) -> Self {
        Self::Io { action, source }
    }
}

#[cfg(test)]
mod tests {
    use super::SYSTEM_SKILLS_DIR;
    use super::collect_fingerprint_items;

    #[test]
    fn fingerprint_traverses_nested_entries() {
        let mut items = Vec::new();
        collect_fingerprint_items(&SYSTEM_SKILLS_DIR, &mut items);
        let mut paths: Vec<String> = items.into_iter().map(|(path, _)| path).collect();
        paths.sort_unstable();

        assert!(
            paths
                .binary_search_by(|probe| probe.as_str().cmp("skill-creator/SKILL.md"))
                .is_ok()
        );
        assert!(
            paths
                .binary_search_by(|probe| probe.as_str().cmp("skill-creator/scripts/init_skill.py"))
                .is_ok()
        );
    }

    #[test]
    fn installation_stages_replacements_and_recovers_previous_tree() {
        use super::*;
        let home = tempfile::tempdir().expect("home");
        let home = AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute home");
        install_system_skills(&home).expect("initial install");
        let dest = system_cache_root_dir(&home);
        let skill = dest.join("skill-creator/SKILL.md");
        for path in [
            "imagegen/SKILL.md",
            "imagegen/references/prompting.md",
            "plugin-creator/SKILL.md",
            "openai-docs/SKILL.md",
            "openai-docs/references/codex-self-knowledge.md",
        ] {
            assert_eq!(
                fs::read(dest.join(path).as_path()).expect("installed instructions"),
                SYSTEM_SKILLS_DIR
                    .get_file(path)
                    .expect("embedded instructions")
                    .contents(),
                "installation must preserve selected skill instructions and linked references: {path}"
            );
        }
        assert_eq!(
            fs::read(skill.as_path()).expect("installed skill"),
            SYSTEM_SKILLS_DIR
                .get_file("skill-creator/SKILL.md")
                .expect("embedded skill")
                .contents()
        );
        let marker = dest.join(SYSTEM_SKILLS_MARKER_FILENAME);
        fs::write(marker.as_path(), "old-version").expect("old marker");
        fs::write(dest.join("old-only.txt").as_path(), "working installation").expect("old file");
        let result = install_system_skills_with(&home, |staged| {
            fs::write(staged.join("partial.txt").as_path(), "partial").expect("partial write");
            Err(SystemSkillsError::io(
                "injected staging failure",
                std::io::Error::other("full disk"),
            ))
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(marker.as_path()).expect("old marker"),
            "old-version"
        );
        assert_eq!(
            fs::read_to_string(dest.join("old-only.txt").as_path()).expect("old tree"),
            "working installation"
        );
        assert!(!dest.join("partial.txt").as_path().exists());
        install_system_skills(&home).expect("replace old tree");
        assert!(!dest.join("old-only.txt").as_path().exists());
        assert_eq!(
            read_marker(&marker).expect("marker"),
            embedded_system_skills_fingerprint()
        );
        install_system_skills_with(&home, |_| panic!("warm installation should skip writing"))
            .expect("warm install");

        let backup = home.join(SKILLS_DIR_NAME).join(".system-backup");
        fs::rename(dest.as_path(), backup.as_path()).expect("simulate interrupted publication");
        install_system_skills(&home).expect("recover previous tree");
        assert!(skill.as_path().is_file());
        assert!(!backup.as_path().exists());
        uninstall_system_skills(&home).expect("uninstall");
        assert!(!dest.as_path().exists());
    }

    #[test]
    fn shared_home_installers_publish_complete_tree() {
        use super::*;
        let home = tempfile::tempdir().expect("home");
        let home = AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute home");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    install_system_skills(&home).expect("concurrent install");
                });
            }
        });
        let dest = system_cache_root_dir(&home);
        let mut items = Vec::new();
        collect_fingerprint_items(&SYSTEM_SKILLS_DIR, &mut items);
        for (path, contents) in items {
            if contents.is_some() {
                assert_eq!(
                    fs::read(dest.join(&path).as_path()).expect("installed file"),
                    SYSTEM_SKILLS_DIR
                        .get_file(&path)
                        .expect("embedded file")
                        .contents()
                );
            } else {
                assert!(dest.join(path).as_path().is_dir());
            }
        }
        assert_eq!(
            read_marker(&dest.join(SYSTEM_SKILLS_MARKER_FILENAME)).expect("marker"),
            embedded_system_skills_fingerprint()
        );
    }
}
