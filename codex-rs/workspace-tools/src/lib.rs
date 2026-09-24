//! Shared build-lane support for isolated workspace transactions.
pub mod validation;

#[cfg(test)]
use std::process::Command;

#[cfg(test)]
pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
}
