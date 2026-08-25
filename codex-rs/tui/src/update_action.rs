#[cfg(not(debug_assertions))]
use codex_install_context::InstallContext;
pub use codex_install_context::UpdateAction;

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}
