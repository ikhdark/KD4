use super::ExternalAgentSessionMigration;
use super::ledger::load_import_ledger;
use super::ledger::session_content_sha256;
use super::now_unix_seconds;
use super::summarize_session;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

const SESSION_IMPORT_MAX_COUNT: usize = 50;
const SESSION_IMPORT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub fn detect_recent_sessions(
    external_agent_home: &Path,
    codex_home: &Path,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    let projects_root = external_agent_home.join("projects");
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let now = now_unix_seconds();
    let ledger = load_import_ledger(codex_home)?;
    let imported_hashes = ledger.imported_hashes_by_source();
    let mut file_candidates = Vec::new();
    for project_entry in fs::read_dir(projects_root)? {
        let Ok(project_entry) = project_entry else {
            continue;
        };
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(project_path) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified_at) = metadata.modified() else {
                continue;
            };
            let Ok(modified_at) = modified_at.duration_since(std::time::UNIX_EPOCH) else {
                continue;
            };
            if (modified_at.as_secs() as i64)
                < now.saturating_sub(SESSION_IMPORT_MAX_AGE.as_secs() as i64)
            {
                continue;
            }
            let Ok(modified_at_nanos) = i64::try_from(modified_at.as_nanos()) else {
                continue;
            };
            file_candidates.push((modified_at_nanos, path));
        }
    }

    file_candidates.sort_unstable_by(
        |(left_modified_at, left_path), (right_modified_at, right_path)| {
            right_modified_at
                .cmp(left_modified_at)
                .then_with(|| left_path.cmp(right_path))
        },
    );
    let mut migrations = Vec::new();
    for (_modified_at, path) in file_candidates {
        // Resolve only candidates reached before the result limit, but still use
        // canonical identities for every ledger lookup.
        let Ok(source_path) = fs::canonicalize(&path) else {
            continue;
        };
        // Content identity, not modification time, decides whether this version was imported.
        if let Some(hashes) = imported_hashes.get(source_path.as_path()) {
            match session_content_sha256(&source_path) {
                Ok(content_sha256) if hashes.contains(content_sha256.as_str()) => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
        }
        let Ok(Some(summary)) = summarize_session(&path) else {
            continue;
        };
        let migration = summary.migration;
        if !migration.cwd.is_dir() {
            continue;
        }
        migrations.push(migration);
        if migrations.len() == SESSION_IMPORT_MAX_COUNT {
            break;
        }
    }

    Ok(migrations)
}

