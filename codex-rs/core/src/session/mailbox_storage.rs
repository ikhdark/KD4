use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::{InterAgentCommunication, RolloutItem};
use codex_protocol::{ResponseItemId, ThreadId};
use codex_thread_store::LiveThread;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::session::SessionStorageOwnership;

// Includes the bounded live mailbox, the transferred prefix, and dedup IDs for
// coalesced progress. Refuse new work rather than evict unrecorded envelopes.
const MAX_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MailboxSnapshot {
    generation: Uuid,
    pub(super) pending: Vec<InterAgentCommunication>,
    pub(super) superseded: VecDeque<ResponseItemId>,
}

impl Default for MailboxSnapshot {
    fn default() -> Self {
        Self {
            generation: Uuid::nil(),
            pending: Vec::new(),
            superseded: VecDeque::new(),
        }
    }
}

pub(super) struct MailboxStorage {
    home: PathBuf,
    id: ThreadId,
    owner: Weak<SessionStorageOwnership>,
    unmaterialized_rollout: Option<LiveThread>,
    pub(super) snapshot: MailboxSnapshot,
}

impl MailboxStorage {
    pub(super) async fn load(
        home: PathBuf,
        id: ThreadId,
        owner: &Arc<SessionStorageOwnership>,
        rollout: &LiveThread,
        needs_materialization: bool,
    ) -> io::Result<Self> {
        let path = codex_thread_store::thread_mailbox_path(&home, id);
        let snapshot = tokio::task::spawn_blocking(move || read_snapshot(&path))
            .await
            .map_err(io::Error::other)??;
        Ok(Self {
            home,
            id,
            owner: Arc::downgrade(owner),
            unmaterialized_rollout: needs_materialization.then(|| rollout.clone()),
            snapshot,
        })
    }

    pub(super) fn same_generation(&self, other: &Self) -> bool {
        self.snapshot.generation == other.snapshot.generation
    }

    /// The client can supply a resume history override. Read the local
    /// receiver's actual rollout under its live storage owner instead.
    pub(super) async fn recorded_ids(
        &self,
        rollout_path: &Path,
    ) -> io::Result<HashSet<ResponseItemId>> {
        let pending: HashMap<_, _> = self.snapshot.pending.iter()
            .filter_map(|mail| mail.id.as_ref().map(|id| (id, mail)))
            .collect();
        let mut recorded = HashSet::new();
        if pending.is_empty() {
            return Ok(recorded);
        }
        let (thread_id, _) = codex_rollout::RolloutRecorder::for_each_rollout_item(rollout_path, |item| {
            let (id, author, recipient) = match &item {
                RolloutItem::ResponseItem(ResponseItem::AgentMessage { id: Some(id), author, recipient, .. }) => {
                    (id, author.as_str(), recipient.as_str())
                }
                RolloutItem::InterAgentCommunication(InterAgentCommunication {
                    id: Some(id), author, recipient, ..
                }) => (id, author.as_str(), recipient.as_str()),
                _ => return,
            };
            if pending.get(id).is_some_and(|mail| {
                mail.author.as_str() == author && mail.recipient.as_str() == recipient
            }) {
                recorded.insert(id.clone());
            }
        }).await?;
        if thread_id != Some(self.id) {
            return Err(io::Error::other("durable mailbox receiver rollout identity changed"));
        }
        Ok(recorded)
    }

    pub(super) async fn confirm_current(&self) -> io::Result<()> {
        let expected = self.snapshot.generation;
        let owner = self.owner.upgrade();
        let home = self.home.clone();
        let id = self.id;
        tokio::task::spawn_blocking(move || {
            let _lease = confirm_current(&home, id, owner.as_ref(), expected)?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) async fn save(&mut self, mut next: MailboxSnapshot) -> io::Result<()> {
        // A freshly created receiver may still have a lazy rollout. Do not
        // acknowledge mail for a receiver whose identity cannot be reopened.
        let owner = self.owner.upgrade();
        if let Some(rollout) = &self.unmaterialized_rollout {
            if owner.is_some() {
                rollout.persist().await.map_err(io::Error::other)?;
                rollout.flush_durable().await.map_err(io::Error::other)?;
            } else if codex_rollout::find_thread_rollout_paths(&self.home, self.id)
                .await?
                .is_empty()
            {
                return Err(io::Error::other(
                    "mailbox receiver closed before its rollout was materialized",
                ));
            }
            self.unmaterialized_rollout = None;
        }
        let expected = self.snapshot.generation;
        next.generation = Uuid::now_v7();
        let home = self.home.clone();
        let id = self.id;
        self.snapshot = tokio::task::spawn_blocking(move || {
            let _lease = confirm_current(&home, id, owner.as_ref(), expected)?;
            let path = codex_thread_store::thread_mailbox_path(&home, id);
            let bytes = serde_json::to_vec(&next)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
                return Err(io::Error::other("durable mailbox storage is full"));
            }
            // Same-directory, sync-before-publish replacement; no torn
            // acceptance/supersession state. KD4's Windows helper syncs the
            // parent where the filesystem allows it.
            codex_file_system::write_bytes_atomically(&path, &bytes)?;
            Ok(next)
        }).await.map_err(io::Error::other)??;
        Ok(())
    }
}

fn confirm_current(
    home: &Path,
    id: ThreadId,
    owner: Option<&Arc<SessionStorageOwnership>>,
    expected: Uuid,
) -> io::Result<Option<codex_file_system::AtomicWriteLock>> {
    let parked_lease = if owner.is_none() {
        // Queue-only delivery can use a retained queue while parked.
        // Share the live session's owner (and hard deletion's lock).
        let path = home.join("tool-history").join(format!("{id}.owner"));
        Some(codex_file_system::try_acquire_atomic_write_lock(&path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "mailbox receiver is attached to another session; retry",
            )
        })?)
    } else {
        None
    };
    if home.join("tool-history").join(format!("{id}.retired")).try_exists()? {
        return Err(io::Error::other("mailbox receiver storage was permanently retired"));
    }
    if parked_lease.is_some()
        && read_snapshot(&codex_thread_store::thread_mailbox_path(home, id))?.generation != expected
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "retained mailbox is stale; reattach the receiver before retrying",
        ));
    }
    Ok(parked_lease)
}

fn read_snapshot(path: &Path) -> io::Result<MailboxSnapshot> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(MailboxSnapshot::default());
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
        return Err(io::Error::other("durable mailbox snapshot exceeds its size limit"));
    }
    let snapshot: MailboxSnapshot = serde_json::from_slice(&bytes)?;
    if snapshot.generation.is_nil() || snapshot.pending.iter().any(|mail| mail.id.is_none()) {
        return Err(io::Error::other("invalid durable mailbox snapshot identity"));
    }
    Ok(snapshot)
}
