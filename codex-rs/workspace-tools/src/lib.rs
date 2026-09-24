//! Sandboxed workers for isolated implementation and compiler-resolved context.
//! These run through the ordinary command runtime, never in the host tool router.
pub mod diagnostics;
pub mod semantic;
pub mod source_units;
pub mod validation;

use anyhow::Context;
use serde_json::Value;
use std::process::Command;

pub const WORKER_ARG: &str = "--codex-workspace-worker";

pub fn run(operation: &str, input: &str) -> anyhow::Result<Value> {
    match operation {
        "semantic_context" => semantic::run(serde_json::from_str(input)?),
        "workspace_validation" => validation::run(serde_json::from_str(input)?),
        _ => anyhow::bail!("unknown workspace worker operation: {operation}"),
    }
}

pub fn main() -> ! {
    let result = (|| {
        let mut args = std::env::args().skip(2);
        let operation = args.next().context("missing worker operation")?;
        let input = args.next().context("missing JSON request")?;
        anyhow::ensure!(args.next().is_none(), "unexpected worker argument");
        run(&operation, &input)
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            std::process::exit(if value["success"] == false { 1 } else { 0 });
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"success":false,"error":format!("{error:#}")})
            );
            std::process::exit(1);
        }
    }
}

pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
}
