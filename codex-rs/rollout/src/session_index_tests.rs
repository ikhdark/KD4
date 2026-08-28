#![allow(warnings, clippy::all)]

use super::*;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Read as _;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

const LOCK_HOLDER_CHILD_TEST: &str = "session_index::tests::session_index_lock_holder_child";
const LOCK_HOLDER_CODEX_HOME_ENV: &str = "CODEX_SESSION_INDEX_LOCK_HOLDER_CODEX_HOME";
const LOCK_HOLDER_READY_PATH_ENV: &str = "CODEX_SESSION_INDEX_LOCK_HOLDER_READY_PATH";
const LOCK_HOLDER_TIMEOUT: Duration = Duration::from_secs(5);

struct SessionIndexLockHolder {
    child: Option<Child>,
}

impl SessionIndexLockHolder {
    fn spawn(codex_home: &Path, ready_file: &Path) -> std::io::Result<Self> {
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(LOCK_HOLDER_CHILD_TEST)
            .arg("--ignored")
            .stdin(Stdio::piped())
            .env(LOCK_HOLDER_CODEX_HOME_ENV, codex_home)
            .env(LOCK_HOLDER_READY_PATH_ENV, ready_file)
            .spawn()?;
        let mut holder = Self { child: Some(child) };
        let started = Instant::now();
        while !ready_file.exists() {
            if let Some(status) = holder.child_mut().try_wait()? {
                holder.child.take();
                return Err(std::io::Error::other(format!(
                    "session index lock holder exited before acquiring the lock: {status}"
                )));
            }
            if started.elapsed() > LOCK_HOLDER_TIMEOUT {
                let timeout_error = std::io::Error::other(
                    "timed out waiting for child process to acquire session index lock",
                );
                return match holder.stop() {
                    Ok(()) => Err(timeout_error),
                    Err(cleanup_error) => Err(std::io::Error::other(format!(
                        "{timeout_error}; child cleanup failed: {cleanup_error}"
                    ))),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(holder)
    }

    fn stop(mut self) -> std::io::Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> std::io::Result<()> {
        let child = self.child_mut();
        drop(child.stdin.take());
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "session index lock holder exited with status {status}"
                    )))
                };
            }
            if started.elapsed() > LOCK_HOLDER_TIMEOUT {
                child.kill()?;
                let status = child.wait()?;
                self.child.take();
                return Err(std::io::Error::other(format!(
                    "timed out waiting for session index lock holder to exit; killed child with status {status}"
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("session index lock holder should still be running")
    }
}

impl Drop for SessionIndexLockHolder {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.stop_inner();
        }
    }
}

fn write_index(path: &Path, lines: &[SessionIndexEntry]) -> std::io::Result<()> {
    let mut out = String::new();
    for entry in lines {
        out.push_str(&serde_json::to_string(entry).unwrap());
        out.push('\n');
    }
    std::fs::write(path, out)
}

fn write_rollout_with_metadata(path: &Path, thread_id: ThreadId) -> std::io::Result<()> {
    let timestamp = "2024-01-01T00-00-00Z".to_string();
    let line = RolloutLine {
        timestamp: timestamp.clone(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                forked_from_id: None,
                parent_thread_id: None,
                timestamp,
                cwd: ".".into(),
                originator: "test_originator".into(),
                cli_version: "test_version".into(),
                source: SessionSource::Cli,
                thread_source: None,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
                model_provider: Some("test-provider".into()),
                base_instructions: None,
                dynamic_tools: None,
                selected_capability_roots: Vec::new(),
                memory_mode: None,
                history_mode: Default::default(),
                multi_agent_version: None,
                context_window: None,
            },
            git: None,
        }),
    };
    let body = serde_json::to_string(&line).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("{body}\n"))
}

#[test]
fn append_waits_for_session_index_lock_held_by_another_process() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let ready_file = temp.path().join("lock-holder-ready");
    let lock_holder = SessionIndexLockHolder::spawn(temp.path(), &ready_file)?;

    let entry = SessionIndexEntry {
        id: ThreadId::new(),
        thread_name: "cross-process".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    let expected = entry.clone();
    let codex_home = temp.path().to_path_buf();
    let (result_tx, result_rx) = mpsc::channel();
    let append_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build Tokio runtime");
        let result = runtime.block_on(append_session_index_entry(&codex_home, &entry));
        let _ = result_tx.send(result);
    });

    let append_was_blocked = matches!(
        result_rx.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    lock_holder.stop()?;
    assert!(
        append_was_blocked,
        "append should wait while another process holds the session index lock"
    );
    result_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(std::io::Error::other)??;
    append_thread
        .join()
        .map_err(|_| std::io::Error::other("append thread panicked"))?;

    let contents = std::fs::read_to_string(session_index_path(temp.path()))?;
    let entries = contents
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<SessionIndexEntry>, _>>()
        .map_err(std::io::Error::other)?;
    assert_eq!(entries, vec![expected]);
    Ok(())
}

#[test]
fn cross_process_lock_wait_does_not_block_another_index() -> std::io::Result<()> {
    let locked_home = TempDir::new()?;
    let other_home = TempDir::new()?;
    let ready_file = locked_home.path().join("lock-holder-ready");
    let lock_holder = SessionIndexLockHolder::spawn(locked_home.path(), &ready_file)?;

    let blocked_entry = SessionIndexEntry {
        id: ThreadId::new(),
        thread_name: "blocked".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    let blocked_home = locked_home.path().to_path_buf();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let blocked_thread = std::thread::spawn(move || {
        let result = append_session_index_entry_blocking(&blocked_home, &blocked_entry);
        let _ = blocked_tx.send(result);
    });
    let blocked_on_held_index = matches!(
        blocked_rx.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    let other_entry = SessionIndexEntry {
        id: ThreadId::new(),
        thread_name: "unrelated".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    let other_home = other_home.path().to_path_buf();
    let (other_tx, other_rx) = mpsc::channel();
    let other_thread = std::thread::spawn(move || {
        let result = append_session_index_entry_blocking(&other_home, &other_entry);
        let _ = other_tx.send(result);
    });
    let other_result = other_rx.recv_timeout(Duration::from_secs(2));
    let other_completed_while_locked = other_result.is_ok();

    let stop_result = lock_holder.stop();
    let blocked_result = blocked_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(std::io::Error::other)?;
    let final_other_result = match other_result {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => other_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(std::io::Error::other)?,
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
            "unrelated append thread disconnected",
        )),
    };
    let blocked_join = blocked_thread
        .join()
        .map_err(|_| std::io::Error::other("blocked append thread panicked"));
    let other_join = other_thread
        .join()
        .map_err(|_| std::io::Error::other("unrelated append thread panicked"));

    stop_result?;
    blocked_result?;
    final_other_result?;
    blocked_join?;
    other_join?;
    assert!(
        blocked_on_held_index,
        "append should wait on the index locked by another process"
    );
    assert!(
        other_completed_while_locked,
        "a cross-process wait for one index must not block an unrelated index"
    );
    Ok(())
}

#[test]
#[ignore = "child process for append_waits_for_session_index_lock_held_by_another_process"]
fn session_index_lock_holder_child() -> std::io::Result<()> {
    let codex_home = std::env::var_os(LOCK_HOLDER_CODEX_HOME_ENV).ok_or_else(|| {
        std::io::Error::other(format!("missing required {LOCK_HOLDER_CODEX_HOME_ENV}"))
    })?;
    let ready_file = std::env::var_os(LOCK_HOLDER_READY_PATH_ENV).ok_or_else(|| {
        std::io::Error::other(format!("missing required {LOCK_HOLDER_READY_PATH_ENV}"))
    })?;

    with_session_index_lock(Path::new(&codex_home), || {
        std::fs::write(ready_file, b"ready")?;
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        Ok(())
    })
}

#[test]
fn find_thread_id_by_name_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id: id1,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id2,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = scan_index_from_end(&path, |entry| entry.thread_name == "same")?;
    assert_eq!(found.map(|entry| entry.id), Some(id2));
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_skips_newest_entry_without_rollout() -> std::io::Result<()> {
    // A newer unsaved name entry should not shadow an older persisted rollout with the same name.
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let saved_id = ThreadId::new();
    let unsaved_id = ThreadId::new();
    let saved_rollout_path = temp
        .path()
        .join("sessions/2024/01/01")
        .join(format!("rollout-2024-01-01T00-00-00-{saved_id}.jsonl"));
    std::fs::create_dir_all(saved_rollout_path.parent().expect("rollout parent"))?;
    write_rollout_with_metadata(&saved_rollout_path, saved_id)?;
    let lines = vec![
        SessionIndexEntry {
            id: saved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: unsaved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(
        found.map(|(path, session_meta)| (path, session_meta.meta.id)),
        Some((saved_rollout_path, saved_id))
    );
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_skips_partial_rollout() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let saved_id = ThreadId::new();
    let partial_id = ThreadId::new();
    let rollout_dir = temp.path().join("sessions/2024/01/01");
    let saved_rollout_path =
        rollout_dir.join(format!("rollout-2024-01-01T00-00-00-{saved_id}.jsonl"));
    let partial_rollout_path =
        rollout_dir.join(format!("rollout-2024-01-01T00-00-01-{partial_id}.jsonl"));
    std::fs::create_dir_all(&rollout_dir)?;
    write_rollout_with_metadata(&saved_rollout_path, saved_id)?;
    std::fs::write(&partial_rollout_path, "")?;
    let lines = vec![
        SessionIndexEntry {
            id: saved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: partial_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(found.map(|(path, _)| path), Some(saved_rollout_path));
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_ignores_historical_name_after_rename() -> std::io::Result<()>
{
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let renamed_id = ThreadId::new();
    let current_id = ThreadId::new();
    let current_rollout_path = temp
        .path()
        .join("sessions/2024/01/01")
        .join(format!("rollout-2024-01-01T00-00-00-{current_id}.jsonl"));
    std::fs::create_dir_all(current_rollout_path.parent().expect("rollout parent"))?;
    write_rollout_with_metadata(&current_rollout_path, current_id)?;
    let lines = vec![
        SessionIndexEntry {
            id: renamed_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: current_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: renamed_id,
            thread_name: "different".to_string(),
            updated_at: "2024-01-03T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(found.map(|(path, _)| path), Some(current_rollout_path));
    Ok(())
}

#[test]
fn find_thread_name_by_id_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id,
            thread_name: "first".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id,
            thread_name: "second".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = scan_index_from_end_by_id(&path, &id)?;
    assert_eq!(
        found.map(|entry| entry.thread_name),
        Some("second".to_string())
    );
    Ok(())
}

#[test]
fn scan_index_returns_none_when_entry_missing() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id = ThreadId::new();
    let lines = vec![SessionIndexEntry {
        id,
        thread_name: "present".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    }];
    write_index(&path, &lines)?;

    let missing_name = scan_index_from_end(&path, |entry| entry.thread_name == "missing")?;
    assert_eq!(missing_name, None);

    let missing_id = scan_index_from_end_by_id(&path, &ThreadId::new())?;
    assert_eq!(missing_id, None);
    Ok(())
}

#[tokio::test]
async fn find_thread_names_by_ids_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id: id1,
            thread_name: "first".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id2,
            thread_name: "other".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id1,
            thread_name: "latest".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let mut ids = HashSet::new();
    ids.insert(id1);
    ids.insert(id2);

    let mut expected = HashMap::new();
    expected.insert(id1, "latest".to_string());
    expected.insert(id2, "other".to_string());

    let found = find_thread_names_by_ids(temp.path(), &ids).await?;
    assert_eq!(found, expected);
    Ok(())
}

#[test]
fn confirmed_performance_batch_name_lookup_stops_after_newest_entries() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let entries = [
        SessionIndexEntry {
            id: id1,
            thread_name: "latest-one".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id2,
            thread_name: "latest-two".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    let mut contents = vec![b'x'; READ_CHUNK_SIZE * 2];
    contents.push(b'\n');
    for entry in &entries {
        contents.extend_from_slice(
            serde_json::to_string(entry)
                .map_err(std::io::Error::other)?
                .as_bytes(),
        );
        contents.push(b'\n');
    }
    let total_bytes = contents.len();
    std::fs::write(&path, contents)?;

    reset_session_index_reverse_bytes_read();
    let found = scan_index_from_end_by_ids(&path, &HashSet::from([id1, id2]))?;
    assert_eq!(
        found,
        HashMap::from([
            (id1, "latest-one".to_string()),
            (id2, "latest-two".to_string()),
        ])
    );
    let bytes_read = session_index_reverse_bytes_read();
    assert!(bytes_read <= READ_CHUNK_SIZE);
    assert!(bytes_read < total_bytes);
    Ok(())
}

#[test]
fn scan_index_finds_latest_match_among_mixed_entries() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id_target = ThreadId::new();
    let id_other = ThreadId::new();
    let expected = SessionIndexEntry {
        id: id_target,
        thread_name: "target".to_string(),
        updated_at: "2024-01-03T00:00:00Z".to_string(),
    };
    let expected_other = SessionIndexEntry {
        id: id_other,
        thread_name: "target".to_string(),
        updated_at: "2024-01-02T00:00:00Z".to_string(),
    };
    // Resolution is based on append order (scan from end), not updated_at.
    let lines = vec![
        SessionIndexEntry {
            id: id_target,
            thread_name: "target".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        expected_other.clone(),
        expected.clone(),
        SessionIndexEntry {
            id: ThreadId::new(),
            thread_name: "another".to_string(),
            updated_at: "2024-01-04T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found_by_name = scan_index_from_end(&path, |entry| entry.thread_name == "target")?;
    assert_eq!(found_by_name, Some(expected.clone()));

    let found_by_id = scan_index_from_end_by_id(&path, &id_target)?;
    assert_eq!(found_by_id, Some(expected));

    let found_other_by_id = scan_index_from_end_by_id(&path, &id_other)?;
    assert_eq!(found_other_by_id, Some(expected_other));
    Ok(())
}
