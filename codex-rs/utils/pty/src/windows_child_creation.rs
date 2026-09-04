use std::io;

/// Scope passed to serialized Windows child-creation operations.
///
/// The scope deliberately exposes no coordinator guard. Callers can only mark
/// the process-wide inheritance state as compromised when exact restoration
/// cannot be proven.
pub struct WindowsChildCreationScope {
    _private: (),
}

#[cfg(windows)]
mod imp {
    use super::WindowsChildCreationScope;
    use std::cell::Cell;
    use std::io;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    static CHILD_CREATION_COORDINATOR: OnceLock<Mutex<()>> = OnceLock::new();
    static INHERITANCE_STATE_COMPROMISED: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static IN_CHILD_CREATION: Cell<bool> = const { Cell::new(false) };
    }

    const COMPROMISED_MESSAGE: &str =
        "Windows child creation is disabled because handle inheritance state is compromised";
    const REENTRY_MESSAGE: &str = "same-thread re-entry into Windows child creation is not allowed";

    pub(super) fn mark_inheritance_state_compromised() {
        INHERITANCE_STATE_COMPROMISED.store(true, Ordering::Release);
    }

    fn ensure_not_compromised() -> io::Result<()> {
        if INHERITANCE_STATE_COMPROMISED.load(Ordering::Acquire) {
            Err(io::Error::other(COMPROMISED_MESSAGE))
        } else {
            Ok(())
        }
    }

    pub(super) fn with_windows_child_creation<T>(
        operation: impl FnOnce(&WindowsChildCreationScope) -> io::Result<T>,
    ) -> io::Result<T> {
        ensure_not_compromised()?;
        if IN_CHILD_CREATION.with(Cell::get) {
            return Err(io::Error::other(REENTRY_MESSAGE));
        }

        emit_test_event(WindowsChildCreationEvent::Attempting);
        let coordinator = CHILD_CREATION_COORDINATOR.get_or_init(|| Mutex::new(()));
        let guard = match coordinator.lock() {
            Ok(guard) => guard,
            Err(_) => {
                mark_inheritance_state_compromised();
                return Err(io::Error::other(format!(
                    "{COMPROMISED_MESSAGE}: coordinator lock poisoned"
                )));
            }
        };
        if let Err(error) = ensure_not_compromised() {
            drop(guard);
            return Err(error);
        }

        IN_CHILD_CREATION.with(|active| active.set(true));
        emit_test_event(WindowsChildCreationEvent::Acquired);
        let _execution = CoordinatorExecution { guard: Some(guard) };
        operation(&WindowsChildCreationScope { _private: () })
    }

    struct CoordinatorExecution<'a> {
        guard: Option<std::sync::MutexGuard<'a, ()>>,
    }

    impl Drop for CoordinatorExecution<'_> {
        fn drop(&mut self) {
            emit_test_event(WindowsChildCreationEvent::Released);
            IN_CHILD_CREATION.with(|active| active.set(false));
            drop(self.guard.take());
        }
    }

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum WindowsChildCreationEvent {
        Attempting,
        Acquired,
        Released,
    }

    #[cfg(not(test))]
    #[derive(Clone, Copy)]
    enum WindowsChildCreationEvent {
        Attempting,
        Acquired,
        Released,
    }

    #[cfg(test)]
    type TestObserver = std::sync::Arc<dyn Fn(WindowsChildCreationEvent) + Send + Sync>;

    #[cfg(test)]
    type StrictHandleObserver = std::sync::Arc<dyn Fn(WindowsStrictHandleEvent) + Send + Sync>;

    #[cfg(test)]
    static TEST_OBSERVER: Mutex<Option<TestObserver>> = Mutex::new(None);
    #[cfg(test)]
    static TEST_OBSERVER_INSTALL: Mutex<()> = Mutex::new(());
    #[cfg(test)]
    static STRICT_HANDLE_OBSERVER: Mutex<Option<StrictHandleObserver>> = Mutex::new(None);
    #[cfg(test)]
    static STRICT_HANDLE_OBSERVER_INSTALL: Mutex<()> = Mutex::new(());

    #[cfg(test)]
    pub(crate) struct WindowsChildCreationObserverGuard {
        _install: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum WindowsStrictHandleEvent {
        Inheritable([usize; 3]),
        Restored {
            handles: [usize; 3],
            flags: [u32; 3],
        },
    }

    #[cfg(test)]
    pub(crate) struct WindowsStrictHandleObserverGuard {
        _install: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(test)]
    pub(crate) fn install_test_observer(
        observer: TestObserver,
    ) -> WindowsChildCreationObserverGuard {
        let install = TEST_OBSERVER_INSTALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *TEST_OBSERVER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
        WindowsChildCreationObserverGuard { _install: install }
    }

    #[cfg(test)]
    impl Drop for WindowsChildCreationObserverGuard {
        fn drop(&mut self) {
            *TEST_OBSERVER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn install_strict_handle_test_observer(
        observer: StrictHandleObserver,
    ) -> WindowsStrictHandleObserverGuard {
        let install = STRICT_HANDLE_OBSERVER_INSTALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *STRICT_HANDLE_OBSERVER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
        WindowsStrictHandleObserverGuard { _install: install }
    }

    #[cfg(test)]
    impl Drop for WindowsStrictHandleObserverGuard {
        fn drop(&mut self) {
            *STRICT_HANDLE_OBSERVER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn emit_strict_handle_test_event(event: WindowsStrictHandleEvent) {
        let observer = STRICT_HANDLE_OBSERVER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer(event);
        }
    }

    #[cfg(test)]
    fn emit_test_event(event: WindowsChildCreationEvent) {
        let observer = TEST_OBSERVER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer(event);
        }
    }

    #[cfg(not(test))]
    fn emit_test_event(_event: WindowsChildCreationEvent) {}
}

impl WindowsChildCreationScope {
    /// Permanently disables coordinated Windows child creation in this process.
    ///
    /// Call this only when a temporary handle-inheritance mutation cannot be
    /// restored exactly. The latch intentionally cannot be reset.
    pub fn mark_inheritance_state_compromised(&self) {
        #[cfg(windows)]
        imp::mark_inheritance_state_compromised();
    }
}

/// Runs one synchronous child-creation operation under the process-wide
/// Windows coordinator.
///
/// On non-Windows platforms the closure runs directly.
pub fn with_windows_child_creation<T>(
    operation: impl FnOnce(&WindowsChildCreationScope) -> io::Result<T>,
) -> io::Result<T> {
    #[cfg(windows)]
    {
        imp::with_windows_child_creation(operation)
    }
    #[cfg(not(windows))]
    {
        operation(&WindowsChildCreationScope { _private: () })
    }
}

#[cfg(all(test, windows))]
pub(crate) use imp::WindowsChildCreationEvent;
#[cfg(all(test, windows))]
pub(crate) use imp::WindowsStrictHandleEvent;
#[cfg(all(test, windows))]
pub(crate) use imp::emit_strict_handle_test_event;
#[cfg(all(test, windows))]
pub(crate) use imp::install_strict_handle_test_observer;
#[cfg(all(test, windows))]
pub(crate) use imp::install_test_observer as install_windows_child_creation_test_observer;

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    const POISON_HELPER_ENV: &str = "KD4_PTY_WINDOWS_CHILD_CREATION_POISON_HELPER";

    #[test]
    fn same_thread_reentry_fails_without_deadlocking() -> io::Result<()> {
        with_windows_child_creation(|_| {
            let error = with_windows_child_creation(|_| Ok(()))
                .expect_err("same-thread coordinator re-entry must fail");
            assert!(error.to_string().contains("same-thread re-entry"));
            Ok(())
        })
    }

    #[test]
    fn poisoned_coordinator_fails_closed_permanently() -> io::Result<()> {
        if std::env::var_os(POISON_HELPER_ENV).is_some() {
            let panic = std::panic::catch_unwind(|| {
                let _ = with_windows_child_creation(|_| -> io::Result<()> {
                    panic!("poison coordinator for fail-closed test")
                });
            });
            assert!(panic.is_err());
            for _ in 0..2 {
                let error = with_windows_child_creation(|_| Ok(()))
                    .expect_err("poisoned coordinator must remain disabled");
                assert!(error.to_string().contains("compromised"));
            }
            return Ok(());
        }

        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("--exact")
            .arg("windows_child_creation::tests::poisoned_coordinator_fails_closed_permanently")
            .arg("--nocapture")
            .env(POISON_HELPER_ENV, "1");
        let mut child = with_windows_child_creation(|_| command.spawn())?;
        let status = child.wait()?;
        assert!(status.success(), "isolated poison helper failed: {status}");
        Ok(())
    }
}
