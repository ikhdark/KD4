//! Admission for native operations with enforced filesystem footprints.
//! Opaque writers exclude everything; readers commute; additions commute only
//! in disjoint, existing directories. A cancelled waiter removes its own claim.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
enum Access {
    Read,
    Write,
    Additions(BTreeSet<PathBuf>),
}

impl Access {
    fn conflicts(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Read, Self::Read) => false,
            (Self::Additions(left), Self::Additions(right)) => left.iter().any(|left| {
                right.iter().any(|right| left.starts_with(right) || right.starts_with(left))
            }),
            _ => true,
        }
    }
}

#[derive(Debug)]
struct Claim {
    identity: Arc<()>,
    access: Access,
    admitted: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ScopedWorkspaceGate {
    claims: Mutex<Vec<Claim>>,
    changed: Notify,
}

#[derive(Debug)]
pub(crate) struct WorkspaceLease {
    gate: Arc<ScopedWorkspaceGate>,
    identity: Arc<()>,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        self.gate.claims.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|claim| !Arc::ptr_eq(&claim.identity, &self.identity));
        self.gate.changed.notify_waiters();
    }
}

impl WorkspaceLease {
    pub(crate) fn downgrade(self) -> Self {
        {
            let mut claims = self.gate.claims.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(claim) = claims.iter_mut()
                .find(|claim| Arc::ptr_eq(&claim.identity, &self.identity))
                && matches!(claim.access, Access::Write)
            {
                claim.access = Access::Read;
            }
        }
        self.gate.changed.notify_waiters();
        self
    }

    #[cfg(test)]
    pub(crate) fn gate(&self) -> Arc<ScopedWorkspaceGate> {
        Arc::clone(&self.gate)
    }
}

impl ScopedWorkspaceGate {
    async fn acquire(self: Arc<Self>, access: Access) -> WorkspaceLease {
        let lease = WorkspaceLease { gate: Arc::clone(&self), identity: Arc::new(()) };
        self.claims.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Claim { identity: Arc::clone(&lease.identity), access, admitted: false });
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before inspecting state so release cannot be lost
            // between that inspection and awaiting the notification.
            changed.as_mut().enable();
            {
                let mut claims = self.claims.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let index = claims.iter().position(|claim| {
                    Arc::ptr_eq(&claim.identity, &lease.identity)
                }).expect("live admission owns its claim");
                // A later disjoint request may pass, but never a conflicting
                // request ahead of an older waiter (including an opaque writer).
                let blocked = claims.iter().enumerate().any(|(other_index, other)| {
                    other_index != index && (other.admitted || other_index < index)
                        && claims[index].access.conflicts(&other.access)
                });
                if !blocked {
                    claims[index].admitted = true;
                    return lease;
                }
            }
            changed.await;
        }
    }

    fn try_acquire(self: Arc<Self>, access: Access) -> Result<WorkspaceLease, ()> {
        let mut claims = self.claims.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if claims.iter().any(|claim| access.conflicts(&claim.access)) {
            return Err(());
        }
        let identity = Arc::new(());
        claims.push(Claim { identity: Arc::clone(&identity), access, admitted: true });
        Ok(WorkspaceLease { gate: Arc::clone(&self), identity })
    }

    pub(crate) async fn read_owned(self: Arc<Self>) -> WorkspaceLease {
        self.acquire(Access::Read).await
    }

    pub(crate) async fn write_owned(self: Arc<Self>) -> WorkspaceLease {
        self.acquire(Access::Write).await
    }

    pub(crate) fn try_read_owned(self: Arc<Self>) -> Result<WorkspaceLease, ()> {
        self.try_acquire(Access::Read)
    }

    pub(crate) fn try_write_owned(self: Arc<Self>) -> Result<WorkspaceLease, ()> {
        self.try_acquire(Access::Write)
    }

    #[cfg(test)]
    pub(crate) fn try_lock(self: &Arc<Self>) -> Result<WorkspaceLease, ()> {
        Arc::clone(self).try_write_owned()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct NativeAdditions {
    root: PathBuf,
    cwd: PathBuf,
    paths: Vec<PathBuf>,
    directories: BTreeSet<PathBuf>,
}

impl NativeAdditions {
    /// Only absent targets under existing canonical directories qualify. No
    /// update, rename, directory creation, metadata write, remote path, or
    /// ambiguous Windows path is granted a scoped mutation lease.
    pub(crate) async fn resolve(root: &Path, cwd: &Path, paths: &[PathBuf]) -> Option<Self> {
        if paths.is_empty() || paths.len() > 64 {
            return None;
        }
        let root = tokio::fs::canonicalize(root).await.ok()?;
        let mut directories = BTreeSet::new();
        for path in paths {
            if path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
                return None;
            }
            let target = cwd.join(path);
            let name = target.file_name()?.to_str()?;
            if !name.is_ascii() || name.contains(':') {
                return None;
            }
            if cfg!(windows) {
                let stem = name.split('.').next()?.to_ascii_uppercase();
                if name.ends_with('.') || name.ends_with(' ')
                    || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || ["COM", "LPT"].iter().any(|prefix| {
                        stem.strip_prefix(*prefix).is_some_and(|suffix| {
                            suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                        })
                    })
                {
                    return None;
                }
            }
            let parent = tokio::fs::canonicalize(target.parent()?).await.ok()?;
            let relative = parent.strip_prefix(&root).ok()?;
            if relative.components().any(|part| part.as_os_str().to_str()
                .is_some_and(|part| part.eq_ignore_ascii_case(".git")))
                || name.eq_ignore_ascii_case(".git")
            {
                return None;
            }
            let parent_text = parent.to_str()?;
            if cfg!(windows) && !parent_text.is_ascii() {
                return None;
            }
            match tokio::fs::symlink_metadata(parent.join(name)).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return None,
            }
            directories.insert(if cfg!(windows) {
                PathBuf::from(parent_text.to_ascii_lowercase())
            } else {
                parent
            });
        }
        Some(Self { root, cwd: cwd.to_path_buf(), paths: paths.to_vec(), directories })
    }

    pub(crate) async fn acquire(self, gate: Arc<ScopedWorkspaceGate>) -> WorkspaceLease {
        let lease = Arc::clone(&gate).acquire(Access::Additions(self.directories.clone())).await;
        // A preceding opaque writer may have replaced a parent or created a
        // target while we waited. Do not retain a stale footprint in that case.
        if Self::resolve(&self.root, &self.cwd, &self.paths).await.as_ref() == Some(&self) {
            lease
        } else {
            drop(lease);
            gate.write_owned().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn native_addition_claims_exclude_overlaps_and_release_cancelled_waiters() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("left")).unwrap();
        std::fs::create_dir(root.path().join("right")).unwrap();
        let paths = |name: &str| vec![PathBuf::from(name)];
        let gate = Arc::new(ScopedWorkspaceGate::default());
        let left = NativeAdditions::resolve(root.path(), root.path(), &paths("left/new"))
            .await.unwrap().acquire(Arc::clone(&gate)).await;
        let right = tokio::time::timeout(Duration::from_secs(1),
            NativeAdditions::resolve(root.path(), root.path(), &paths("right/new"))
                .await.unwrap().acquire(Arc::clone(&gate)),
        ).await.expect("disjoint additions must not wait");
        assert!(Arc::clone(&gate).try_read_owned().is_err());
        assert!(Arc::clone(&gate).try_write_owned().is_err());
        assert!(tokio::time::timeout(Duration::from_millis(20),
            NativeAdditions::resolve(root.path(), root.path(), &paths("left/other"))
                .await.unwrap().acquire(Arc::clone(&gate)),
        ).await.is_err(), "same-directory waiter must remain excluded");
        // The timed-out waiter must not leave a stale queue claim.
        drop(left);
        let left = tokio::time::timeout(Duration::from_secs(1),
            NativeAdditions::resolve(root.path(), root.path(), &paths("left/other"))
                .await.unwrap().acquire(Arc::clone(&gate)),
        ).await.unwrap();
        drop(left);
        drop(right);
        let exclusive = Arc::clone(&gate).try_write_owned().unwrap();
        drop(exclusive);
        let first_reader = Arc::clone(&gate).try_read_owned().unwrap();
        let second_reader = Arc::clone(&gate).try_read_owned().unwrap();
        drop((first_reader, second_reader));
        std::fs::write(root.path().join("left/existing"), "existing").unwrap();
        for path in ["left/existing", "missing/new", ".git", "left/../right/new"] {
            assert!(NativeAdditions::resolve(root.path(), root.path(), &paths(path)).await.is_none());
        }
    }
}
