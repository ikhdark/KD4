use super::setup_mutex::SetupMutexGuard;
use super::setup_mutex::acquire_named_setup_mutex;
use anyhow::Result;

const READ_ACL_MUTEX_NAME: &str = "Local\\CodexSandboxReadAcl";

pub(super) fn acquire_read_acl_mutex() -> Result<SetupMutexGuard> {
    // An existing helper may be processing different roots. Wait for it, then
    // recheck this request's ACLs rather than treating contention as completion.
    acquire_named_setup_mutex(READ_ACL_MUTEX_NAME)
}
