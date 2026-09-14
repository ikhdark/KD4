use anyhow::Result;
use anyhow::anyhow;
use std::env;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let output_path = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow!("missing output path argument"))?,
    );
    let payload = args
        .next()
        .ok_or_else(|| anyhow!("missing payload argument"))?
        .into_string()
        .map_err(|_| anyhow!("payload must be valid UTF-8"))?;

    anyhow::ensure!(args.next().is_none(), "expected payload as final argument");
    let mut temp_name = output_path.as_os_str().to_os_string();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    std::fs::write(&temp_path, payload)?;
    std::fs::rename(&temp_path, &output_path)?;

    Ok(())
}
