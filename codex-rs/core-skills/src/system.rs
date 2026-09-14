pub(crate) use codex_skills::install_system_skills;
pub(crate) use codex_skills::system_cache_root_dir;

use codex_utils_absolute_path::AbsolutePathBuf;

pub(crate) fn uninstall_system_skills(codex_home: &AbsolutePathBuf) {
    let _ = codex_skills::uninstall_system_skills(codex_home);
}
